//! Per-host circuit breaker — quarantines a host after 3 failures of
//! the same category within 10 minutes. Reads/writes host_circuit_status
//! (V107).
//!
//! The dispatcher (#145) checks `is_quarantined` before assigning work;
//! the watchdog (#160) calls `record_failure` after every task failure.
//!
//! ## Provider-level breaker (V149)
//!
//! A cloud-provider outage is NOT a host fault — a claude `529` hits every
//! host using claude — so the `*_provider_*` fns below break per
//! (computer, provider) against `fleet_backend_health`, using the ff-council
//! thresholds: trip on ≥5 transient failures in a 5-min window (or ≥50% over
//! ≥10 requests); cooldown 2 min for overload/5xx, 10 min for
//! rate-limit/quota; half-open after cooldown, close after 4 consecutive
//! successes, any failure reopens. The headroom router consults
//! `is_provider_open` before picking a backend.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Record a usage-headroom signal for a (computer, provider) into
/// `fleet_provider_usage`, which the headroom router (`pg_routed_backends`)
/// reads. `remaining_pct` is the best estimate from the most recent call
/// outcome: 100 on a clean success, low on a rate-limit/overload, 0 on quota
/// exhaustion. A single `live` row per (computer, provider) is overwritten each
/// call so the latest outcome wins — self-correcting (a later success lifts a
/// provider back above the router's headroom floor). The dispatch path has no
/// HTTP headers to read, so the call OUTCOME is the signal.
pub async fn record_usage_signal(
    pool: &PgPool,
    computer_id: Uuid,
    provider: &str,
    remaining_pct: f64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO fleet_provider_usage
            (computer_id, provider, used_pct, remaining_pct, window_kind, source, sampled_at)
         VALUES ($1, $2, 100 - $3, $3, 'live', 'dispatch_outcome', NOW())
         ON CONFLICT (computer_id, provider, window_kind) DO UPDATE SET
            used_pct = 100 - $3, remaining_pct = $3, source = 'dispatch_outcome', sampled_at = NOW()",
    )
    .bind(computer_id)
    .bind(provider)
    .bind(remaining_pct)
    .execute(pool)
    .await?;
    Ok(())
}

/// Map a cloud-error category to the headroom it implies, when the error is a
/// usage/capacity signal. `None` for errors that say nothing about quota (auth,
/// bad-request, etc.). Quota → 0 (skip), rate-limit/overload → below the
/// router's 15% floor so the provider is deprioritized until it recovers.
pub fn headroom_hint_for_category(category: &str) -> Option<f64> {
    match category {
        "quota_exhausted" => Some(0.0),
        "rate_limited" => Some(8.0),
        "overloaded" => Some(12.0),
        _ => None,
    }
}

/// Council cooldown (minutes) by failure category. Rate-limit / quota take a
/// longer rest than a transient overload/5xx blip.
fn provider_cooldown_minutes(category: &str) -> i64 {
    match category {
        "rate_limited" | "quota_exhausted" => 10,
        _ => 2,
    }
}

/// Record a provider-level failure against `fleet_backend_health`. Returns
/// `true` if the breaker tripped open on this call. The 5-min rolling window
/// resets when stale. Trip condition (council): ≥5 errors in the window, OR
/// ≥50% error rate over ≥10 requests.
pub async fn record_provider_failure(
    pool: &PgPool,
    computer_id: Uuid,
    provider: &str,
    category: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query(
        "INSERT INTO fleet_backend_health
            (computer_id, provider, recent_error_count, recent_req_count,
             window_start, last_error_class, last_error_at, updated_at)
         VALUES ($1, $2, 1, 1, NOW(), $3, NOW(), NOW())
         ON CONFLICT (computer_id, provider) DO UPDATE SET
            recent_error_count = CASE
                WHEN fleet_backend_health.window_start < NOW() - INTERVAL '5 minutes'
                THEN 1 ELSE fleet_backend_health.recent_error_count + 1 END,
            recent_req_count = CASE
                WHEN fleet_backend_health.window_start < NOW() - INTERVAL '5 minutes'
                THEN 1 ELSE fleet_backend_health.recent_req_count + 1 END,
            window_start = CASE
                WHEN fleet_backend_health.window_start < NOW() - INTERVAL '5 minutes'
                THEN NOW() ELSE fleet_backend_health.window_start END,
            last_error_class = $3, last_error_at = NOW(), updated_at = NOW()",
    )
    .bind(computer_id)
    .bind(provider)
    .bind(category)
    .execute(pool)
    .await?;

    let (errs, reqs): (i32, i32) = sqlx::query_as(
        "SELECT recent_error_count, recent_req_count FROM fleet_backend_health
          WHERE computer_id = $1 AND provider = $2",
    )
    .bind(computer_id)
    .bind(provider)
    .fetch_one(pool)
    .await?;

    let trip = errs >= 5 || (reqs >= 10 && (errs as f64) / (reqs as f64) >= 0.5);
    if trip {
        let until = Utc::now() + Duration::minutes(provider_cooldown_minutes(category));
        sqlx::query(
            "UPDATE fleet_backend_health
                SET breaker_state = 'open', breaker_open_until = $3,
                    half_open_successes = 0, updated_at = NOW()
              WHERE computer_id = $1 AND provider = $2",
        )
        .bind(computer_id)
        .bind(provider)
        .bind(until)
        .execute(pool)
        .await?;
    }
    Ok(trip)
}

/// True if the provider's breaker is OPEN (not usable) right now. When the
/// cooldown has elapsed, transitions `open → half_open` and returns `false`
/// so a single probe request can flow through.
pub async fn is_provider_open(
    pool: &PgPool,
    computer_id: Uuid,
    provider: &str,
) -> Result<bool, sqlx::Error> {
    let row: Option<(String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT breaker_state, breaker_open_until FROM fleet_backend_health
          WHERE computer_id = $1 AND provider = $2",
    )
    .bind(computer_id)
    .bind(provider)
    .fetch_optional(pool)
    .await?;

    let Some((state, until)) = row else {
        return Ok(false); // no record → healthy
    };
    if state == "closed" {
        return Ok(false);
    }
    if state == "open" {
        if until.map(|u| u <= Utc::now()).unwrap_or(true) {
            // Cooldown elapsed → half-open; let one probe through.
            sqlx::query(
                "UPDATE fleet_backend_health
                    SET breaker_state = 'half_open', half_open_successes = 0, updated_at = NOW()
                  WHERE computer_id = $1 AND provider = $2 AND breaker_state = 'open'",
            )
            .bind(computer_id)
            .bind(provider)
            .execute(pool)
            .await?;
            return Ok(false);
        }
        return Ok(true); // still cooling down
    }
    Ok(false) // half_open → allow probes
}

/// Record a provider-level success. In `half_open` this counts toward the
/// 4-consecutive-success close; in `closed` it just advances the request
/// window so the failure-rate denominator stays honest.
pub async fn record_provider_success(
    pool: &PgPool,
    computer_id: Uuid,
    provider: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO fleet_backend_health (computer_id, provider, recent_req_count, updated_at)
         VALUES ($1, $2, 1, NOW())
         ON CONFLICT (computer_id, provider) DO UPDATE SET
            recent_req_count = fleet_backend_health.recent_req_count + 1,
            half_open_successes = CASE
                WHEN fleet_backend_health.breaker_state = 'half_open'
                THEN fleet_backend_health.half_open_successes + 1 ELSE 0 END,
            breaker_state = CASE
                WHEN fleet_backend_health.breaker_state = 'half_open'
                     AND fleet_backend_health.half_open_successes + 1 >= 4
                THEN 'closed' ELSE fleet_backend_health.breaker_state END,
            breaker_open_until = CASE
                WHEN fleet_backend_health.breaker_state = 'half_open'
                     AND fleet_backend_health.half_open_successes + 1 >= 4
                THEN NULL ELSE fleet_backend_health.breaker_open_until END,
            updated_at = NOW()",
    )
    .bind(computer_id)
    .bind(provider)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_failure(
    pool: &PgPool,
    worker_name: &str,
    category: &str,
) -> Result<bool, sqlx::Error> {
    let count: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM task_failures tf
           JOIN fleet_tasks t ON t.id = tf.task_id
           JOIN computers  c ON c.id = t.claimed_by_computer_id
          WHERE c.name = $1
            AND tf.category = $2
            AND tf.occurred_at > NOW() - INTERVAL '10 minutes'",
    )
    .bind(worker_name)
    .bind(category)
    .fetch_one(pool)
    .await?;
    if count.0 >= 3 {
        let opens_until = Utc::now() + Duration::minutes(15);
        sqlx::query(
            "INSERT INTO host_circuit_status (worker_name, failure_category, opens_until, reason)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (worker_name, failure_category) DO UPDATE
             SET opens_until = EXCLUDED.opens_until, reason = EXCLUDED.reason",
        )
        .bind(worker_name)
        .bind(category)
        .bind(opens_until)
        .bind("3+ failures in 10 min")
        .execute(pool)
        .await?;
        return Ok(true);
    }
    Ok(false)
}

pub async fn is_quarantined(pool: &PgPool, worker_name: &str) -> Result<bool, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT count(*) FROM host_circuit_status WHERE worker_name = $1 AND opens_until > NOW()",
    )
    .bind(worker_name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|c| c.0 > 0).unwrap_or(false))
}
