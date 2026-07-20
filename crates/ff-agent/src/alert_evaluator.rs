//! Alert evaluator — runs on the leader every 60s.
//!
//! Loads `alert_policies`, evaluates each against Pulse beats + recent
//! `computer_metrics_history`, and fires/resolves `alert_events`.
//! Dispatches through a minimal channel switch:
//!   - `log`     : tracing::warn!
//!   - `telegram`: best-effort `openclaw agent --channel telegram --to ...`
//!   - `webhook` : POST JSON to `fleet_secrets.alert_webhook_url`
//!   - `openclaw`: same as telegram
//!
//! Condition grammar is deliberately tiny so the evaluator stays obvious:
//!   - `> N`      — numeric greater-than
//!   - `< N`      — numeric less-than
//!   - `>= N`, `<= N`, `== N` — numeric comparisons
//!   - `== 'str'` or `!= 'str'` — string comparisons (for `computer_status`)

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use sqlx::{PgPool, Row};

use crate::notifications::SHARED_HTTP;
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use ff_pulse::reader::{PulseError, PulseReader};

#[derive(Debug, Error)]
pub enum AlertError {
    #[error("pulse: {0}")]
    Pulse(#[from] PulseError),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Per-tick summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct EvalReport {
    pub policies_evaluated: usize,
    pub events_fired: usize,
    pub events_resolved: usize,
}

/// One row from `alert_policies`.
#[derive(Debug, Clone)]
struct Policy {
    id: Uuid,
    name: String,
    metric: String,
    scope: String,
    scope_computer_id: Option<Uuid>,
    condition: String,
    duration_secs: i32,
    severity: String,
    cooldown_secs: i32,
    channel: String,
}

/// Parsed condition.
#[derive(Debug, Clone)]
enum Condition {
    NumGt(f64),
    NumLt(f64),
    NumGe(f64),
    NumLe(f64),
    NumEq(f64),
    StrEq(String),
    StrNe(String),
    /// Condition we couldn't parse; every evaluation is treated as false so
    /// nothing fires.
    Unparseable,
}

fn parse_condition(raw: &str) -> Condition {
    let s = raw.trim();
    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = s.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = s.strip_prefix("==") {
        ("==", r)
    } else if let Some(r) = s.strip_prefix("!=") {
        ("!=", r)
    } else if let Some(r) = s.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = s.strip_prefix('<') {
        ("<", r)
    } else {
        return Condition::Unparseable;
    };
    let arg = rest.trim();

    // String literal?
    if (arg.starts_with('\'') && arg.ends_with('\'') && arg.len() >= 2)
        || (arg.starts_with('"') && arg.ends_with('"') && arg.len() >= 2)
    {
        let inner = &arg[1..arg.len() - 1];
        return match op {
            "==" => Condition::StrEq(inner.to_string()),
            "!=" => Condition::StrNe(inner.to_string()),
            _ => Condition::Unparseable,
        };
    }

    // Numeric.
    let Ok(n) = arg.parse::<f64>() else {
        return Condition::Unparseable;
    };
    match op {
        ">" => Condition::NumGt(n),
        "<" => Condition::NumLt(n),
        ">=" => Condition::NumGe(n),
        "<=" => Condition::NumLe(n),
        "==" => Condition::NumEq(n),
        _ => Condition::Unparseable,
    }
}

fn eval_numeric(cond: &Condition, v: f64) -> bool {
    match cond {
        Condition::NumGt(n) => v > *n,
        Condition::NumLt(n) => v < *n,
        Condition::NumGe(n) => v >= *n,
        Condition::NumLe(n) => v <= *n,
        Condition::NumEq(n) => (v - *n).abs() < f64::EPSILON,
        _ => false,
    }
}

fn eval_string(cond: &Condition, v: &str) -> bool {
    match cond {
        Condition::StrEq(s) => v == s,
        Condition::StrNe(s) => v != s,
        _ => false,
    }
}

/// Metrics whose live value is read from the computer's current Pulse beat.
/// When the beat is ABSENT (computer offline / beat key expired) the value is
/// UNKNOWN — not `0` — so [`evaluate_current`] must not evaluate a content
/// condition against a phantom `0`. Otherwise the seeded `low_disk_space`
/// policy (`disk_free_gb < 20`) fires for every offline computer (`0 < 20`),
/// even though its disk is fine — it's just unreachable. Down/offline detection
/// is handled separately by `computer_status` (absent beat → `sdown`) and the
/// DB-sourced `beat_age_secs` / `leader_heartbeat_age_secs` metrics, which are
/// deliberately NOT in this set.
fn is_beat_sourced_numeric(metric: &str) -> bool {
    matches!(
        metric,
        "cpu_pct"
            | "ram_pct"
            | "ram_used_gb"
            | "disk_free_gb"
            | "gpu_pct"
            | "llm_queue_depth"
            | "llm_active_requests"
            | "llm_tokens_per_sec"
    )
}

/// Metrics the evaluator poll (`evaluate_current`) knows how to read each tick
/// from a Pulse beat or the DB. KEEP IN SYNC with the `match metric` arms in
/// `evaluate_current` — a policy on a metric NOT in this list (and not in
/// [`IMPERATIVE_METRICS`]) silently never fires.
pub const EVALUATOR_METRICS: &[&str] = &[
    "cpu_pct",
    "ram_pct",
    "ram_used_gb",
    "disk_free_gb",
    "gpu_pct",
    "llm_queue_depth",
    "llm_active_requests",
    "llm_tokens_per_sec",
    "computer_status",
    "leader_heartbeat_age_secs",
    "beat_age_secs",
];

/// The complete value domain the `computer_status` metric can emit (see the
/// `computer_status` arm of `evaluate_current`). KEEP IN SYNC with it. A
/// `== '<x>'` policy whose `<x>` is outside this set can never match — exactly
/// the `computer_offline == 'odown'` dead-policy bug (V146).
pub const COMPUTER_STATUS_VALUES: &[&str] = &["offline", "online", "sdown"];

/// Metrics whose policies are fired IMPERATIVELY by a dedicated leader tick
/// (the tick resolves the policy by name, then INSERTs an `alert_event` and
/// calls [`dispatch_alert`]) rather than by the evaluator poll. KEEP IN SYNC
/// with the `POLICY_NAME`/metric used in `db_integrity`, `fleet_integrity`,
/// `ha::restore_drill`, `upgrade_rollout`, and `secrets_rotation`.
pub const IMPERATIVE_METRICS: &[&str] = &[
    "db_index_corruption",
    "fleet_integrity_degraded",
    "backup_restore_drill_failed",
    "upgrade_rollout_halted",
    "secret_expiry_days_remaining",
    "ssh_mesh_asymmetric",
];

/// How (or whether) a policy can ever fire — the result of [`classify_policy_fireability`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyFireability {
    /// Metric is read by the evaluator poll each tick.
    EvaluatorPolled,
    /// Fired by a dedicated imperative tick (resolved by policy name).
    ImperativelyFired,
    /// Condition string doesn't parse → every evaluation is false → never fires.
    Unparseable,
    /// Metric is handled by neither path → watches nothing → never fires.
    UnknownMetric,
    /// Known metric, parseable condition, but a `== '<x>'` test against a value
    /// the metric can never emit (e.g. `computer_status == 'odown'`).
    UnsatisfiableValue,
}

impl PolicyFireability {
    /// True when the policy is structurally CAPABLE of firing (a typo'd metric,
    /// unparseable condition, or unsatisfiable value test is not).
    pub fn can_fire(self) -> bool {
        matches!(self, Self::EvaluatorPolled | Self::ImperativelyFired)
    }
}

/// Classify whether a policy can ever fire from its `metric` + `condition`.
/// Pure — the engine behind `ff alert doctor`, which flags the dead-policy
/// class (e.g. the `computer_offline == 'odown'` policy disabled in V146).
pub fn classify_policy_fireability(metric: &str, condition: &str) -> PolicyFireability {
    let cond = parse_condition(condition);
    if matches!(cond, Condition::Unparseable) {
        return PolicyFireability::Unparseable;
    }
    // Value-domain check for the one enum-valued metric: `== '<x>'` where `<x>`
    // is outside what the metric can emit can never match. (`!= '<x>'` is fine —
    // it just always matches — so only StrEq is unsatisfiable here.)
    if metric == "computer_status"
        && let Condition::StrEq(v) = &cond
        && !COMPUTER_STATUS_VALUES.contains(&v.as_str())
    {
        return PolicyFireability::UnsatisfiableValue;
    }
    if EVALUATOR_METRICS.contains(&metric) {
        PolicyFireability::EvaluatorPolled
    } else if IMPERATIVE_METRICS.contains(&metric) {
        PolicyFireability::ImperativelyFired
    } else {
        PolicyFireability::UnknownMetric
    }
}

/// Default suppression window for repeated Telegram alerts sharing the same
/// (metric, node) combination.
const TELEGRAM_ALERT_THROTTLE_TTL: Duration = Duration::from_secs(3600);

/// In-process throttle for Telegram alerts.
///
/// Repeated alerts for the same (metric, node) combination are suppressed
/// for the configured TTL so that flapping conditions (or a fleet-wide
/// incident that resolves and re-fires) do not flood the operator chat.
#[derive(Debug)]
struct TelegramAlertThrottle {
    ttl: Duration,
    seen: DashMap<(String, String), Instant>,
}

impl TelegramAlertThrottle {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            seen: DashMap::new(),
        }
    }

    /// Return `true` if enough time has passed since the last Telegram alert
    /// for this (metric, node) key, and record this send as the most recent.
    fn should_send(&self, metric: &str, node: &str) -> bool {
        use dashmap::mapref::entry::Entry;

        let key = (metric.to_string(), node.to_string());
        let now = Instant::now();
        match self.seen.entry(key) {
            Entry::Occupied(mut entry) => {
                if now.duration_since(*entry.get()) >= self.ttl {
                    entry.insert(now);
                    true
                } else {
                    false
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(now);
                true
            }
        }
    }
}

static TELEGRAM_ALERT_THROTTLE: LazyLock<TelegramAlertThrottle> =
    LazyLock::new(|| TelegramAlertThrottle::new(TELEGRAM_ALERT_THROTTLE_TTL));

/// Extract the `metric=` and `computer=` values from an alert message so the
/// Telegram path can throttle repeated alerts by (metric, node).
fn parse_alert_metric_node(message: &str) -> Option<(String, String)> {
    let mut metric = None;
    let mut node = None;
    for token in message.split_whitespace() {
        if let Some(v) = token.strip_prefix("metric=") {
            metric = Some(v.to_string());
        } else if let Some(v) = token.strip_prefix("computer=") {
            node = Some(v.to_string());
        }
    }
    Some((metric?, node?))
}

pub struct AlertEvaluator {
    pg: PgPool,
    pulse: PulseReader,
}

impl AlertEvaluator {
    pub fn new(pg: PgPool, pulse: PulseReader, _my_name: String) -> Self {
        Self { pg, pulse }
    }

    async fn is_leader(&self) -> bool {
        crate::leader_cache::is_current_leader()
    }

    /// Run one evaluation pass.
    pub async fn evaluate_once(&self) -> Result<EvalReport, AlertError> {
        let mut report = EvalReport::default();

        let policies = load_enabled_policies(&self.pg).await?;
        report.policies_evaluated = policies.len();

        // Deploy mute window: `ff fleet deploy` restarts every forgefleetd, so
        // beat ages legitimately spike past the stale threshold mid-deploy and
        // the evaluator used to spam one member_stale_beat per host per deploy
        // (operator-reported 2026-07-01). handle_fleet_deploy stamps
        // fleet_secrets.alert_mute_until (epoch secs) for the deploy window and
        // clears it on completion; while inside the window, PRESENCE policies
        // (beat age / heartbeat / status) are skipped. Metric-scoped on purpose:
        // resource alerts (cpu/ram/disk) still fire during deploys.
        let presence_muted: bool = sqlx::query_scalar::<_, bool>(
            "SELECT COALESCE(
                 (SELECT NULLIF(value, '')::BIGINT
                    FROM fleet_secrets WHERE key = 'alert_mute_until'),
                 0) > EXTRACT(EPOCH FROM NOW())::BIGINT",
        )
        .fetch_one(&self.pg)
        .await
        .unwrap_or(false);

        // Snapshot beats once per tick so every policy sees the same world.
        let beats = self.pulse.all_beats().await?;

        for policy in &policies {
            if presence_muted
                && matches!(
                    policy.metric.as_str(),
                    "beat_age_secs" | "leader_heartbeat_age_secs" | "computer_status"
                )
            {
                tracing::debug!(
                    policy = %policy.name,
                    "alert mute window active (fleet deploy in progress); skipping presence alert"
                );
                continue;
            }
            let cond = parse_condition(&policy.condition);
            if matches!(cond, Condition::Unparseable) {
                tracing::warn!(
                    policy = %policy.name,
                    condition = %policy.condition,
                    "unparseable alert condition; skipping"
                );
                continue;
            }

            // Resolve target computers by scope.
            let targets = match policy.scope.as_str() {
                "any_computer" => {
                    // All known computers currently beating, or sdown by name.

                    list_all_computers(&self.pg).await?
                }
                "specific" => {
                    if let Some(id) = policy.scope_computer_id {
                        let row: Option<(Uuid, String)> =
                            sqlx::query_as("SELECT id, name FROM computers WHERE id = $1")
                                .bind(id)
                                .fetch_optional(&self.pg)
                                .await?;
                        row.into_iter().collect()
                    } else {
                        Vec::new()
                    }
                }
                "leader_only" => {
                    let row: Option<(Uuid, String)> = sqlx::query_as(
                        r#"
                        SELECT c.id, c.name
                        FROM fleet_leader_state ls
                        JOIN computers c ON c.id = ls.computer_id
                        LIMIT 1
                        "#,
                    )
                    .fetch_optional(&self.pg)
                    .await?;
                    row.into_iter().collect()
                }
                _ => Vec::new(),
            };

            for (computer_id, computer_name) in targets {
                let (cur_matches, cur_value_num, cur_value_str) =
                    evaluate_current(&policy.metric, &cond, &computer_name, &beats, &self.pg)
                        .await?;

                // Find any existing unresolved event for this (policy, computer).
                let unresolved_row: Option<(Uuid, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
                    "SELECT id, fired_at FROM alert_events
                     WHERE policy_id = $1
                       AND (computer_id = $2 OR (computer_id IS NULL AND $2 IS NULL))
                       AND resolved_at IS NULL
                     ORDER BY fired_at DESC LIMIT 1",
                )
                .bind(policy.id)
                .bind(computer_id)
                .fetch_optional(&self.pg)
                .await?;

                if cur_matches {
                    // Condition is TRUE now.
                    if unresolved_row.is_some() {
                        // Already firing. Nothing to do (dispatch happens on transition).
                        continue;
                    }

                    // Cooldown: has this (policy, computer) recently resolved?
                    let recent_resolved: Option<(chrono::DateTime<chrono::Utc>,)> = sqlx::query_as(
                        "SELECT resolved_at FROM alert_events
                         WHERE policy_id = $1
                           AND (computer_id = $2 OR (computer_id IS NULL AND $2 IS NULL))
                           AND resolved_at IS NOT NULL
                         ORDER BY resolved_at DESC LIMIT 1",
                    )
                    .bind(policy.id)
                    .bind(computer_id)
                    .fetch_optional(&self.pg)
                    .await?;

                    if let Some((resolved_at,)) = recent_resolved {
                        let age = chrono::Utc::now()
                            .signed_duration_since(resolved_at)
                            .num_seconds();
                        if age < policy.cooldown_secs as i64 {
                            tracing::debug!(
                                policy = %policy.name,
                                computer = %computer_name,
                                cooldown_remaining = policy.cooldown_secs as i64 - age,
                                "alert within cooldown; not firing"
                            );
                            continue;
                        }
                    }

                    // Check duration — does the condition hold for the last N seconds?
                    if policy.duration_secs > 0
                        && !condition_held_for_duration(
                            &self.pg,
                            computer_id,
                            &policy.metric,
                            &cond,
                            policy.duration_secs as i64,
                        )
                        .await?
                    {
                        tracing::debug!(
                            policy = %policy.name,
                            computer = %computer_name,
                            "condition currently matches but duration not satisfied"
                        );
                        continue;
                    }

                    // Fire!
                    let message = format!(
                        "[{}] {}: computer={} metric={} condition={} value={}",
                        policy.severity,
                        policy.name,
                        computer_name,
                        policy.metric,
                        policy.condition,
                        cur_value_str
                            .clone()
                            .or_else(|| cur_value_num.map(|v| v.to_string()))
                            .unwrap_or_else(|| "(n/a)".into())
                    );

                    let channel_result =
                        dispatch_alert(&self.pg, &policy.channel, &policy.severity, &message).await;

                    sqlx::query(
                        r#"
                        INSERT INTO alert_events (policy_id, computer_id, value, value_text, message, channel_result)
                        VALUES ($1, $2, $3, $4, $5, $6)
                        "#,
                    )
                    .bind(policy.id)
                    .bind(computer_id)
                    .bind(cur_value_num)
                    .bind(cur_value_str.as_deref())
                    .bind(&message)
                    .bind(&channel_result)
                    .execute(&self.pg)
                    .await?;

                    tracing::warn!(
                        policy = %policy.name,
                        computer = %computer_name,
                        channel = %policy.channel,
                        channel_result = %channel_result,
                        "alert fired"
                    );

                    report.events_fired += 1;
                } else if let Some((event_id, _)) = unresolved_row {
                    // Condition no longer true → resolve.
                    sqlx::query("UPDATE alert_events SET resolved_at = NOW() WHERE id = $1")
                        .bind(event_id)
                        .execute(&self.pg)
                        .await?;

                    tracing::info!(
                        policy = %policy.name,
                        computer = %computer_name,
                        "alert resolved"
                    );
                    report.events_resolved += 1;
                }
            }
        }

        Ok(report)
    }

    /// Spawn the 60s evaluator loop. Gate leadership outside this task.
    pub fn spawn(self, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if !self.is_leader().await {
                            continue;
                        }
                        match self.evaluate_once().await {
                            Ok(report) => {
                                tracing::debug!(
                                    policies = report.policies_evaluated,
                                    fired = report.events_fired,
                                    resolved = report.events_resolved,
                                    "alert evaluator tick"
                                );
                            }
                            Err(err) => {
                                tracing::warn!(error = %err, "alert evaluator tick failed");
                            }
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            tracing::info!("alert evaluator shutting down");
                            break;
                        }
                    }
                }
            }
        })
    }
}

// ─── helpers ────────────────────────────────────────────────────────────

async fn load_enabled_policies(pg: &PgPool) -> Result<Vec<Policy>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT id, name, metric, scope, scope_computer_id, condition,
               duration_secs, severity, cooldown_secs, channel
        FROM alert_policies
        WHERE enabled = true
        ORDER BY name
        "#,
    )
    .fetch_all(pg)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| Policy {
            id: r.get("id"),
            name: r.get("name"),
            metric: r.get("metric"),
            scope: r.get("scope"),
            scope_computer_id: r.get("scope_computer_id"),
            condition: r.get("condition"),
            duration_secs: r.get("duration_secs"),
            severity: r.get("severity"),
            cooldown_secs: r.get("cooldown_secs"),
            channel: r.get("channel"),
        })
        .collect())
}

async fn list_all_computers(pg: &PgPool) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
    sqlx::query_as::<_, (Uuid, String)>("SELECT id, name FROM computers ORDER BY name")
        .fetch_all(pg)
        .await
}

/// Evaluate the policy's current state for one computer. Returns
/// `(matches, numeric_value, string_value)`.
async fn evaluate_current(
    metric: &str,
    cond: &Condition,
    computer_name: &str,
    beats: &[ff_pulse::beat_v2::PulseBeatV2],
    pg: &PgPool,
) -> Result<(bool, Option<f64>, Option<String>), AlertError> {
    let beat = beats.iter().find(|b| b.computer_name == computer_name);

    // No beat → this computer's live metrics are UNKNOWN, not 0. Evaluating a
    // beat-sourced numeric condition against a phantom 0 false-fires content
    // alerts for offline computers (e.g. `low_disk_space: disk_free_gb < 20`).
    // Report no-match; offline detection is `computer_status`/`beat_age_secs`.
    if beat.is_none() && is_beat_sourced_numeric(metric) {
        return Ok((false, None, None));
    }

    match metric {
        "cpu_pct" => {
            let v = beat.map(|b| b.load.cpu_pct).unwrap_or(0.0);
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        "ram_pct" => {
            let v = beat.map(|b| b.load.ram_pct).unwrap_or(0.0);
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        "ram_used_gb" => {
            let v = beat.map(|b| b.memory.ram_used_gb).unwrap_or(0.0);
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        "disk_free_gb" => {
            let v = beat.map(|b| b.load.disk_free_gb).unwrap_or(0.0);
            // A live beat reporting exactly 0.0 free disk is a sampling
            // artifact from failed disk-stat collection, not a real full disk
            // (Taylor false-paged at 2:48 AM with 2.2 TB actually free). Real
            // near-full disks still report a small non-zero value and fire.
            if v == 0.0 {
                return Ok((false, None, None));
            }
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        "gpu_pct" => {
            let v = beat.map(|b| b.load.gpu_pct).unwrap_or(0.0);
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        "llm_queue_depth" => {
            let v: f64 = beat
                .map(|b| b.llm_servers.iter().map(|s| s.queue_depth as f64).sum())
                .unwrap_or(0.0);
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        "llm_active_requests" => {
            let v: f64 = beat
                .map(|b| b.llm_servers.iter().map(|s| s.active_requests as f64).sum())
                .unwrap_or(0.0);
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        "llm_tokens_per_sec" => {
            let v: f64 = beat
                .map(|b| {
                    b.llm_servers
                        .iter()
                        .map(|s| s.tokens_per_sec_last_min)
                        .sum()
                })
                .unwrap_or(0.0);
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        "computer_status" => {
            // Emits 'offline' (beat present, going_offline set) | 'online' (beat
            // present) | 'sdown' (no beat). NB: never 'odown' — that quorum
            // status has no producer, which is why the V34 `computer_offline`
            // policy (== 'odown') was disabled in V146; real down-detection is
            // the numeric `beat_age_secs` policies.
            let status = match beat {
                Some(b) if b.going_offline => "offline",
                Some(_) => "online",
                None => "sdown",
            };
            Ok((eval_string(cond, status), None, Some(status.to_string())))
        }
        "leader_heartbeat_age_secs" => {
            let age: Option<(i64,)> = sqlx::query_as(
                "SELECT EXTRACT(EPOCH FROM (NOW() - heartbeat_at))::BIGINT
                 FROM fleet_leader_state LIMIT 1",
            )
            .fetch_optional(pg)
            .await
            .ok()
            .flatten();
            let v = age.map(|(n,)| n as f64).unwrap_or(0.0);
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        "beat_age_secs" => {
            // Per-computer pulse beat age, computed from computers.last_seen_at
            // so the signal survives Redis TTL expiry (45s). Fires even when
            // the beat key has vanished — exactly the scenario ODOWN quorum
            // fails to catch when many peers die simultaneously.
            let age: Option<(i64,)> = sqlx::query_as(
                "SELECT EXTRACT(EPOCH FROM (NOW() - last_seen_at))::BIGINT
                 FROM computers WHERE name = $1",
            )
            .bind(computer_name)
            .fetch_optional(pg)
            .await
            .ok()
            .flatten();
            let v = age.map(|(n,)| n as f64).unwrap_or(0.0);
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        _ => Ok((false, None, None)),
    }
}

/// Quick check: has the metric satisfied the condition in every row for
/// `computer_id` over the last `duration_secs`? We require at least one
/// row AND every recent row to match.
async fn condition_held_for_duration(
    pg: &PgPool,
    computer_id: Uuid,
    metric: &str,
    cond: &Condition,
    duration_secs: i64,
) -> Result<bool, sqlx::Error> {
    let col = match metric {
        "cpu_pct" => "cpu_pct",
        "ram_pct" => "ram_pct",
        "ram_used_gb" => "ram_used_gb",
        "disk_free_gb" => "disk_free_gb",
        "gpu_pct" => "gpu_pct",
        "llm_queue_depth" => "llm_queue_depth",
        "llm_active_requests" => "llm_active_requests",
        "llm_tokens_per_sec" => "llm_tokens_per_sec",
        // Non-numeric metrics: fall back to "trust current value" (no history).
        _ => return Ok(true),
    };

    // `col` is derived from a hardcoded whitelist above; safe to interpolate.
    let sql = format!(
        "SELECT {col}::FLOAT8 AS v FROM computer_metrics_history
         WHERE computer_id = $1
           AND recorded_at > NOW() - ($2 || ' seconds')::interval
         ORDER BY recorded_at ASC
         LIMIT 1000"
    );
    let rows = sqlx::query(&sql)
        .bind(computer_id)
        .bind(duration_secs.to_string())
        .fetch_all(pg)
        .await?;

    if rows.is_empty() {
        // No history yet — can't prove duration, but also can't disprove.
        // Be conservative: require history before firing any duration-gated alert.
        return Ok(false);
    }

    for r in rows {
        let v_opt: Option<f64> = r.try_get("v").ok();
        let v = v_opt.unwrap_or(0.0);
        if !eval_numeric(cond, v) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Dispatch an alert via the configured channel and return a status string
/// for `alert_events.channel_result`. Never returns an error — dispatch
/// failures are recorded in the status string so they show up in listings.
///
/// `pub` so other leader-gated ticks (e.g. the amcheck integrity guard in
/// `db_integrity`) can dispatch alerts directly instead of writing
/// `channel_result='pending'` rows that nothing ever picks up.
pub async fn dispatch_alert(pg: &PgPool, channel: &str, severity: &str, message: &str) -> String {
    match channel {
        "log" => {
            match severity {
                "critical" => tracing::error!(target: "alerts", "{message}"),
                "warning" => tracing::warn!(target: "alerts", "{message}"),
                _ => tracing::info!(target: "alerts", "{message}"),
            }
            "sent".to_string()
        }
        "telegram" => {
            // Throttle repeated Telegram alerts for the same (metric, node)
            // combination. This keeps the operator chat usable during flapping
            // conditions without suppressing the underlying alert_event rows.
            if let Some((metric, node)) = parse_alert_metric_node(message) {
                if !TELEGRAM_ALERT_THROTTLE.should_send(&metric, &node) {
                    tracing::debug!(
                        metric = %metric,
                        node = %node,
                        "telegram alert throttled; recent alert for this metric/node still within TTL"
                    );
                    return "throttled".to_string();
                }
            }

            let chat_id = ff_db::pg_get_secret(pg, "telegram_chat_id")
                .await
                .ok()
                .flatten();
            let Some(chat_id) = chat_id else {
                return "failed: no telegram chat id secret".into();
            };

            let bot_token = ff_db::pg_get_secret(pg, "telegram_bot_token")
                .await
                .ok()
                .flatten();
            let Some(token) = bot_token else {
                return "failed: no bot token configured".into();
            };

            let url = format!("https://api.telegram.org/bot{token}/sendMessage");
            let payload = serde_json::json!({
                "chat_id": chat_id,
                "text": format!("[{severity}] {message}"),
                "disable_web_page_preview": true,
            });
            match SHARED_HTTP
                .post(&url)
                .json(&payload)
                .timeout(Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => "sent".into(),
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    format!("failed: telegram HTTP {status}: {}", body.trim())
                }
                Err(e) => format!("failed: telegram: {e}"),
            }
        }
        "webhook" => {
            let url = ff_db::pg_get_secret(pg, "alert_webhook_url")
                .await
                .ok()
                .flatten();
            let Some(url) = url else {
                return "failed: no alert_webhook_url secret".into();
            };
            let payload = serde_json::json!({
                "severity": severity,
                "message": message,
            });
            match SHARED_HTTP
                .post(&url)
                .json(&payload)
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => "sent".into(),
                Ok(resp) => format!("failed: webhook HTTP {}", resp.status()),
                Err(e) => format!("failed: webhook: {e}"),
            }
        }
        other => format!("failed: unknown channel '{other}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_conditions() {
        assert!(matches!(parse_condition("> 90"), Condition::NumGt(n) if (n - 90.0).abs() < 1e-9));
        assert!(matches!(parse_condition("< 10"), Condition::NumLt(n) if (n - 10.0).abs() < 1e-9));
        assert!(matches!(parse_condition(">= 5"), Condition::NumGe(n) if (n - 5.0).abs() < 1e-9));
        assert!(matches!(parse_condition("<= 2.5"), Condition::NumLe(n) if (n - 2.5).abs() < 1e-9));
    }

    #[test]
    fn parses_string_conditions() {
        match parse_condition("== 'odown'") {
            Condition::StrEq(s) => assert_eq!(s, "odown"),
            _ => panic!("expected StrEq"),
        }
        match parse_condition("!= \"online\"") {
            Condition::StrNe(s) => assert_eq!(s, "online"),
            _ => panic!("expected StrNe"),
        }
    }

    #[test]
    fn unparseable_returns_no_match() {
        let c = parse_condition("no operator here");
        assert!(matches!(c, Condition::Unparseable));
        assert!(!eval_numeric(&c, 100.0));
        assert!(!eval_string(&c, "anything"));
    }

    #[test]
    fn classify_policy_fireability_cases() {
        // Evaluator-polled metric, valid condition.
        assert_eq!(
            classify_policy_fireability("cpu_pct", "> 90"),
            PolicyFireability::EvaluatorPolled
        );
        // String condition on an evaluator metric.
        assert_eq!(
            classify_policy_fireability("computer_status", "!= 'online'"),
            PolicyFireability::EvaluatorPolled
        );
        // Imperatively-fired metric.
        assert_eq!(
            classify_policy_fireability("db_index_corruption", "> 0"),
            PolicyFireability::ImperativelyFired
        );
        // Unknown metric — typo or unimplemented → cannot fire.
        assert_eq!(
            classify_policy_fireability("cpu_percent", "> 90"),
            PolicyFireability::UnknownMetric
        );
        // Known metric but garbage condition → cannot fire (Unparseable wins).
        assert_eq!(
            classify_policy_fireability("cpu_pct", "definitely not a condition"),
            PolicyFireability::Unparseable
        );
        // The motivating bug: computer_status == 'odown' (odown is never emitted).
        assert_eq!(
            classify_policy_fireability("computer_status", "== 'odown'"),
            PolicyFireability::UnsatisfiableValue
        );
        // A status value the metric DOES emit is fine.
        assert_eq!(
            classify_policy_fireability("computer_status", "== 'sdown'"),
            PolicyFireability::EvaluatorPolled
        );
        // != against an out-of-domain value always matches → fireable, not dead.
        assert_eq!(
            classify_policy_fireability("computer_status", "!= 'odown'"),
            PolicyFireability::EvaluatorPolled
        );
        assert!(!PolicyFireability::UnsatisfiableValue.can_fire());
        // can_fire() only for the two real firing paths.
        assert!(PolicyFireability::EvaluatorPolled.can_fire());
        assert!(PolicyFireability::ImperativelyFired.can_fire());
        assert!(!PolicyFireability::UnknownMetric.can_fire());
        assert!(!PolicyFireability::Unparseable.can_fire());
    }

    #[test]
    fn every_live_seeded_metric_can_fire() {
        // Guards the EVALUATOR_METRICS/IMPERATIVE_METRICS lists against drift:
        // every metric the V34+ seed ships must be classifiable as fireable, or
        // `ff alert doctor` would (correctly) flag a seeded policy as dead.
        for metric in [
            "cpu_pct",
            "disk_free_gb",
            "llm_queue_depth",
            "leader_heartbeat_age_secs",
            "beat_age_secs",
            "db_index_corruption",
            "fleet_integrity_degraded",
            "secret_expiry_days_remaining",
            "backup_restore_drill_failed",
            "upgrade_rollout_halted",
            "ssh_mesh_asymmetric",
        ] {
            assert!(
                classify_policy_fireability(metric, "> 0").can_fire(),
                "seeded metric {metric} classified as non-fireable",
            );
        }
    }

    #[test]
    fn beat_sourced_numeric_excludes_status_and_db_metrics() {
        // Beat-sourced numeric metrics: a phantom 0 on absent beat would
        // false-fire content alerts, so these short-circuit to no-match.
        assert!(is_beat_sourced_numeric("disk_free_gb"));
        assert!(is_beat_sourced_numeric("cpu_pct"));
        assert!(is_beat_sourced_numeric("llm_tokens_per_sec"));
        // computer_status maps absent-beat to "sdown" itself — must NOT
        // short-circuit, or `computer_status == 'sdown'` would never fire.
        assert!(!is_beat_sourced_numeric("computer_status"));
        // DB-sourced age metrics read their own value with no beat.
        assert!(!is_beat_sourced_numeric("beat_age_secs"));
        assert!(!is_beat_sourced_numeric("leader_heartbeat_age_secs"));
        // Unknown metric.
        assert!(!is_beat_sourced_numeric("nonsense"));
    }

    #[test]
    fn beat_sourced_numeric_partitions_evaluator_metrics() {
        // Drift guard: every EVALUATOR_METRIC is beat-sourced-numeric EXCEPT the
        // three that source their value elsewhere (status enum + DB age). If a
        // new beat-sourced metric is added to the poll, this forces updating
        // is_beat_sourced_numeric so absent-beat handling stays correct.
        let non_beat_numeric = [
            "computer_status",
            "leader_heartbeat_age_secs",
            "beat_age_secs",
        ];
        for metric in EVALUATOR_METRICS {
            let expected = !non_beat_numeric.contains(metric);
            assert_eq!(
                is_beat_sourced_numeric(metric),
                expected,
                "metric {metric} misclassified",
            );
        }
    }

    #[tokio::test]
    async fn disk_free_zero_sample_does_not_fire_low_disk() {
        let pg = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://forgefleet:forgefleet@localhost/forgefleet")
            .expect("valid lazy postgres URL");
        let cond = parse_condition("< 20");

        let mut zero_sample = ff_pulse::beat_v2::PulseBeatV2::skeleton("taylor");
        zero_sample.load.disk_free_gb = 0.0;
        let (matches, numeric_value, string_value) =
            evaluate_current("disk_free_gb", &cond, "taylor", &[zero_sample], &pg)
                .await
                .expect("disk_free_gb evaluation should not hit the DB");
        assert!(!matches);
        assert_eq!(numeric_value, None);
        assert_eq!(string_value, None);

        let mut low_nonzero = ff_pulse::beat_v2::PulseBeatV2::skeleton("taylor");
        low_nonzero.load.disk_free_gb = 5.0;
        let (matches, numeric_value, string_value) =
            evaluate_current("disk_free_gb", &cond, "taylor", &[low_nonzero], &pg)
                .await
                .expect("disk_free_gb evaluation should not hit the DB");
        assert!(matches);
        assert_eq!(numeric_value, Some(5.0));
        assert_eq!(string_value, None);
    }

    #[test]
    fn eval_numeric_correct() {
        assert!(eval_numeric(&Condition::NumGt(90.0), 91.0));
        assert!(!eval_numeric(&Condition::NumGt(90.0), 90.0));
        assert!(eval_numeric(&Condition::NumLt(10.0), 5.0));
        assert!(eval_numeric(&Condition::NumLe(10.0), 10.0));
    }

    #[test]
    fn parse_alert_metric_node_extracts_fields() {
        let msg = "[warning] high_cpu: computer=taylor metric=cpu_pct condition=> 90 value=95";
        assert_eq!(
            parse_alert_metric_node(msg),
            Some(("cpu_pct".to_string(), "taylor".to_string()))
        );
    }

    #[test]
    fn parse_alert_metric_node_returns_none_when_fields_missing() {
        assert!(parse_alert_metric_node("metric=cpu_pct value=95").is_none());
        assert!(parse_alert_metric_node("computer=taylor value=95").is_none());
        assert!(parse_alert_metric_node("plain message").is_none());
    }

    #[test]
    fn telegram_alert_throttle_suppresses_repeated_metric_node() {
        let throttle = TelegramAlertThrottle::new(Duration::from_secs(60));
        assert!(throttle.should_send("cpu_pct", "taylor"));
        assert!(!throttle.should_send("cpu_pct", "taylor"));
        assert!(throttle.should_send("cpu_pct", "james"));
        assert!(throttle.should_send("ram_pct", "taylor"));
    }

    #[test]
    fn telegram_alert_throttle_allows_after_ttl() {
        let throttle = TelegramAlertThrottle::new(Duration::from_millis(1));
        assert!(throttle.should_send("cpu_pct", "taylor"));
        std::thread::sleep(Duration::from_millis(5));
        assert!(throttle.should_send("cpu_pct", "taylor"));
    }
}
