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

use std::time::Duration;

use sqlx::{PgPool, Row};
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

pub struct AlertEvaluator {
    pg: PgPool,
    pulse: PulseReader,
    my_name: String,
}

impl AlertEvaluator {
    pub fn new(pg: PgPool, pulse: PulseReader, my_name: String) -> Self {
        Self { pg, pulse, my_name }
    }

    async fn is_leader(&self) -> bool {
        match sqlx::query_scalar::<_, String>(
            "SELECT member_name FROM fleet_leader_state LIMIT 1",
        )
        .fetch_optional(&self.pg)
        .await
        {
            Ok(Some(leader)) => leader == self.my_name,
            Ok(None) => false,
            Err(_) => false,
        }
    }

    /// Run one evaluation pass.
    pub async fn evaluate_once(&self) -> Result<EvalReport, AlertError> {
        let mut report = EvalReport::default();

        let policies = load_enabled_policies(&self.pg).await?;
        report.policies_evaluated = policies.len();

        // Snapshot beats once per tick so every policy sees the same world.
        let beats = self.pulse.all_beats().await?;

        for policy in &policies {
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
                    let all_computers = list_all_computers(&self.pg).await?;
                    all_computers
                }
                "specific" => {
                    if let Some(id) = policy.scope_computer_id {
                        let row: Option<(Uuid, String)> = sqlx::query_as(
                            "SELECT id, name FROM computers WHERE id = $1",
                        )
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
                let (cur_matches, cur_value_num, cur_value_str) = evaluate_current(
                    &policy.metric,
                    &cond,
                    &computer_name,
                    &beats,
                    &self.pg,
                )
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

                    let channel_result = dispatch_alert(
                        &self.pg,
                        &policy.channel,
                        &policy.severity,
                        &message,
                    )
                    .await;

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
                    sqlx::query(
                        "UPDATE alert_events SET resolved_at = NOW() WHERE id = $1",
                    )
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
                .map(|b| b.llm_servers.iter().map(|s| s.tokens_per_sec_last_min).sum())
                .unwrap_or(0.0);
            Ok((eval_numeric(cond, v), Some(v), None))
        }
        "computer_status" => {
            // 'odown' | 'sdown' | 'online'
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

    let sql = format!(
        "SELECT {col}::FLOAT8 AS v FROM computer_metrics_history
         WHERE computer_id = $1
           AND recorded_at > NOW() - ($2 || ' seconds')::interval
         ORDER BY recorded_at ASC"
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
async fn dispatch_alert(pg: &PgPool, channel: &str, severity: &str, message: &str) -> String {
    match channel {
        "log" => {
            match severity {
                "critical" => tracing::error!(target: "alerts", "{message}"),
                "warning" => tracing::warn!(target: "alerts", "{message}"),
                _ => tracing::info!(target: "alerts", "{message}"),
            }
            "sent".to_string()
        }
        "telegram" | "openclaw" => {
            let chat_id = ff_db::pg_get_secret(pg, "openclaw.telegram_chat_id")
                .await
                .ok()
                .flatten();
            let Some(chat_id) = chat_id else {
                return "failed: no openclaw.telegram_chat_id secret".into();
            };
            let output = std::process::Command::new("openclaw")
                .args([
                    "agent",
                    "--channel",
                    "telegram",
                    "--to",
                    &chat_id,
                    &format!("ALERT: {message}"),
                ])
                .output();
            match output {
                Ok(out) if out.status.success() => "sent".into(),
                Ok(out) => format!(
                    "failed: openclaw exit {:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                Err(e) => format!("failed: spawn: {e}"),
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
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default();
            match client.post(&url).json(&payload).send().await {
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
    fn eval_numeric_correct() {
        assert!(eval_numeric(&Condition::NumGt(90.0), 91.0));
        assert!(!eval_numeric(&Condition::NumGt(90.0), 90.0));
        assert!(eval_numeric(&Condition::NumLt(10.0), 5.0));
        assert!(eval_numeric(&Condition::NumLe(10.0), 10.0));
    }
}
