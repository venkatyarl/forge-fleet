//! HA leader-handoff Phase 3 — DB-primary-aware handoff PLANNER + executor.
//!
//! Implements the §4 ordering from `plans/ha-leader-handoff.md`:
//!
//!   1. Replica-lag gate — confirm the target hosts a Postgres replica that is
//!      caught up (`lag_bytes` ≈ 0 in `database_replicas`).
//!   2. Promote the target's PG replica → primary (via the EXISTING
//!      `pg_failover::PostgresFailoverManager::promote_local_replica`; this
//!      module NEVER issues a raw destructive `pg_ctl`/SQL itself).
//!   3. Repoint the DSN of record (`dsn_of_record` table + `db_dsn_of_record`
//!      fleet_secret) to the new primary.
//!   4. Move fleet leadership via the EXISTING Phase 2 maintenance lease
//!      (`pg_set_maintenance_lease`).
//!   5. Fail-back is the reverse, run as a fresh handoff back to the old leader.
//!
//! ## SAFETY — gated + operator-triggered, NO automatic failover
//!
//! This module is reached ONLY from `ff fleet db handoff`, never from a tick.
//! The CLI is **dry-run by default**: it prints the plan + lag check and stops.
//! Execution requires an explicit `--execute --yes`, AND even then the global
//! `ha_handoff_mode` fleet_secret must read `active` (fail-safe to off/disabled
//! on missing/unknown, exactly like `disk_reconcile::read_mode`). There is no
//! tick-driven entry point; nothing here runs on its own.

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// `fleet_secrets` key holding the handoff gate. Off / missing = disabled.
/// Mirrors the three-mode pattern used by `disk_reconcile`/autoscaler, but the
/// handoff is ALSO operator-triggered, so even `active` does nothing without an
/// explicit `--execute` on the CLI.
pub const HANDOFF_MODE_KEY: &str = "ha_handoff_mode";

/// Maximum replica lag (bytes) tolerated for a SAFE handoff. A replica more than
/// this far behind the primary would lose committed data on promotion, so the
/// gate refuses. 256 KiB ≈ a handful of WAL records — effectively "caught up"
/// without demanding an exact zero (which races the next commit).
pub const MAX_SAFE_LAG_BYTES: i64 = 256 * 1024;

/// The handoff gate's modes. Same shape + fail-safe as `DiskPolicyMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffMode {
    /// Default + value on missing/unknown: handoff `--execute` is refused.
    Disabled,
    /// Operator has opted in; `--execute --yes` may proceed.
    Active,
}

impl HandoffMode {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("active") => HandoffMode::Active,
            // Off, disabled, missing, empty, or any unrecognised value → safe.
            _ => HandoffMode::Disabled,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HandoffMode::Disabled => "disabled",
            HandoffMode::Active => "active",
        }
    }
}

/// Read the handoff gate. DEFAULTS TO DISABLED on missing/unparseable/error.
pub async fn read_handoff_mode(pool: &PgPool) -> HandoffMode {
    match ff_db::pg_get_secret(pool, HANDOFF_MODE_KEY).await {
        Ok(v) => HandoffMode::parse(v.as_deref()),
        Err(_) => HandoffMode::Disabled,
    }
}

// ─── Pure decision logic (unit-tested) ──────────────────────────────────────

/// The current Postgres role of a candidate computer w.r.t. handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaState {
    pub member: String,
    /// `database_replicas.role` — expected `replica` for a handoff target.
    pub role: String,
    /// `database_replicas.lag_bytes`; `None` when never measured.
    pub lag_bytes: Option<i64>,
}

/// Outcome of the replica-lag gate — the §4 step-1 decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LagGate {
    /// Target hosts a caught-up replica — safe to promote.
    Ok { lag_bytes: i64 },
    /// Target hosts a replica but it is too far behind.
    LagTooHigh { lag_bytes: i64, max: i64 },
    /// Target has no replica row at all.
    NoReplica,
    /// Target hosts a replica but its lag has never been measured (NULL) —
    /// refuse rather than guess.
    LagUnknown,
    /// Target's row exists but is not a `replica` (e.g. already `primary`).
    WrongRole { role: String },
}

impl LagGate {
    /// True only for [`LagGate::Ok`] — the single state that permits promotion.
    pub fn is_safe(&self) -> bool {
        matches!(self, LagGate::Ok { .. })
    }

    pub fn explain(&self) -> String {
        match self {
            LagGate::Ok { lag_bytes } => {
                format!("replica caught up (lag {lag_bytes} B ≤ {MAX_SAFE_LAG_BYTES} B)")
            }
            LagGate::LagTooHigh { lag_bytes, max } => {
                format!("replica too far behind (lag {lag_bytes} B > {max} B) — would lose data")
            }
            LagGate::NoReplica => "target hosts no Postgres replica row".to_string(),
            LagGate::LagUnknown => {
                "replica lag has never been measured (NULL) — refusing to guess".to_string()
            }
            LagGate::WrongRole { role } => {
                format!("target's database_replicas role is '{role}', not 'replica'")
            }
        }
    }
}

/// Evaluate the §4 step-1 replica-lag gate against a candidate's [`ReplicaState`].
/// `state == None` means no `database_replicas` row exists for the target.
///
/// Pure — no DB, no clock — so it is exhaustively unit-testable.
pub fn evaluate_lag_gate(state: Option<&ReplicaState>, max_lag_bytes: i64) -> LagGate {
    let Some(s) = state else {
        return LagGate::NoReplica;
    };
    if s.role != "replica" {
        return LagGate::WrongRole {
            role: s.role.clone(),
        };
    }
    match s.lag_bytes {
        None => LagGate::LagUnknown,
        Some(lag) if lag <= max_lag_bytes => LagGate::Ok { lag_bytes: lag },
        Some(lag) => LagGate::LagTooHigh {
            lag_bytes: lag,
            max: max_lag_bytes,
        },
    }
}

/// One ordered step of a handoff plan, for display + (in `--execute`) actuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub order: u8,
    pub title: String,
    pub detail: String,
    /// `true` when this step performs a non-reversible/stateful action (used to
    /// flag what `--execute` would actually do vs. pure checks).
    pub mutates: bool,
}

/// Inputs needed to build a handoff plan. All resolved up-front so the builder
/// is a pure function (no DB) and therefore unit-testable.
#[derive(Debug, Clone)]
pub struct PlanInputs {
    pub target_member: String,
    pub target_ip: String,
    pub current_primary_member: Option<String>,
    pub current_leader_member: Option<String>,
    pub new_dsn: String,
    /// Maintenance-lease window in minutes (fed to `pg_set_maintenance_lease`).
    pub lease_minutes: i64,
    pub lag_gate: LagGate,
}

/// A complete, ORDERED handoff plan. `safe` is true iff the lag gate passed; an
/// unsafe plan is still rendered (so the operator sees exactly why it's blocked)
/// but `--execute` refuses to run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffPlan {
    pub target_member: String,
    pub safe: bool,
    pub blocking_reason: Option<String>,
    pub steps: Vec<PlanStep>,
}

/// Build the §4-ordered plan from resolved inputs. PURE — unit-tested.
///
/// Step ordering is fixed: lag-gate (check) → promote → repoint DSN → move
/// leadership lease. The promote/lease steps are flagged `mutates = true`.
pub fn build_plan(inputs: &PlanInputs) -> HandoffPlan {
    let safe = inputs.lag_gate.is_safe();
    let blocking_reason = if safe {
        None
    } else {
        Some(inputs.lag_gate.explain())
    };

    let primary = inputs
        .current_primary_member
        .clone()
        .unwrap_or_else(|| "<unknown>".to_string());
    let leader = inputs
        .current_leader_member
        .clone()
        .unwrap_or_else(|| "<unknown>".to_string());

    let steps = vec![
        PlanStep {
            order: 1,
            title: "Replica-lag gate".to_string(),
            detail: format!(
                "target '{}': {}",
                inputs.target_member,
                inputs.lag_gate.explain()
            ),
            mutates: false,
        },
        PlanStep {
            order: 2,
            title: "Promote Postgres replica → primary".to_string(),
            detail: format!(
                "demote '{primary}', promote '{}' ({}) via ha::pg_failover (pg_ctl promote — \
                 NO raw SQL)",
                inputs.target_member, inputs.target_ip
            ),
            mutates: true,
        },
        PlanStep {
            order: 3,
            title: "Repoint DSN of record".to_string(),
            detail: format!(
                "set dsn_of_record + db_dsn_of_record fleet_secret → {}",
                redact_dsn(&inputs.new_dsn)
            ),
            mutates: true,
        },
        PlanStep {
            order: 4,
            title: "Move fleet leadership (maintenance lease)".to_string(),
            detail: format!(
                "lease standby '{}' from current leader '{leader}' for {} min \
                 (Phase 2 pg_set_maintenance_lease)",
                inputs.target_member, inputs.lease_minutes
            ),
            mutates: true,
        },
    ];

    HandoffPlan {
        target_member: inputs.target_member.clone(),
        safe,
        blocking_reason,
        steps,
    }
}

/// Redact the password segment of a `postgres://user:pass@host/db` DSN for
/// display in plans/logs.
pub fn redact_dsn(dsn: &str) -> String {
    // postgres://user:pass@host:port/db  →  postgres://user:***@host:port/db
    if let Some(scheme_end) = dsn.find("://") {
        let (scheme, rest) = dsn.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            let creds = &rest[..at];
            let tail = &rest[at..];
            if let Some(colon) = creds.find(':') {
                let user = &creds[..colon];
                return format!("{scheme}{user}:***{tail}");
            }
        }
    }
    dsn.to_string()
}

// ─── DB-touching resolution (NOT unit-tested; only invoked from the CLI) ─────

/// Look up a computer by name → `(id, name, primary_ip)`.
pub async fn resolve_member(
    pool: &PgPool,
    name: &str,
) -> Result<Option<(Uuid, String, String)>, sqlx::Error> {
    let row =
        sqlx::query("SELECT id, name, primary_ip FROM computers WHERE LOWER(name) = LOWER($1)")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| (r.get("id"), r.get("name"), r.get("primary_ip"))))
}

/// Read the target's `database_replicas` row into a [`ReplicaState`].
pub async fn fetch_replica_state(
    pool: &PgPool,
    computer_id: Uuid,
    member: &str,
) -> Result<Option<ReplicaState>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT role, lag_bytes FROM database_replicas
          WHERE computer_id = $1 AND database_kind = 'postgres'",
    )
    .bind(computer_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| ReplicaState {
        member: member.to_string(),
        role: r.get("role"),
        lag_bytes: r.get("lag_bytes"),
    }))
}

/// Name of the current Postgres primary member, if any.
pub async fn current_primary_member(pool: &PgPool) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT c.name FROM database_replicas dr
            JOIN computers c ON c.id = dr.computer_id
          WHERE dr.database_kind = 'postgres' AND dr.role = 'primary'
          LIMIT 1",
    )
    .fetch_optional(pool)
    .await
}

/// Name of the current fleet leader, if the singleton row exists.
pub async fn current_leader_member(pool: &PgPool) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT member_name FROM fleet_leader_state WHERE singleton_key = 'current'")
        .fetch_optional(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replica(role: &str, lag: Option<i64>) -> ReplicaState {
        ReplicaState {
            member: "james".to_string(),
            role: role.to_string(),
            lag_bytes: lag,
        }
    }

    // ── gate-mode parsing (fail-safe) ──────────────────────────────────────
    #[test]
    fn mode_defaults_to_disabled() {
        assert_eq!(HandoffMode::parse(None), HandoffMode::Disabled);
        assert_eq!(HandoffMode::parse(Some("")), HandoffMode::Disabled);
        assert_eq!(HandoffMode::parse(Some("off")), HandoffMode::Disabled);
        assert_eq!(HandoffMode::parse(Some("garbage")), HandoffMode::Disabled);
        assert_eq!(HandoffMode::parse(Some("ACTIVE")), HandoffMode::Active);
        assert_eq!(HandoffMode::parse(Some(" active ")), HandoffMode::Active);
    }

    // ── replica-lag gate decision ──────────────────────────────────────────
    #[test]
    fn gate_no_row_is_no_replica() {
        assert_eq!(
            evaluate_lag_gate(None, MAX_SAFE_LAG_BYTES),
            LagGate::NoReplica
        );
    }

    #[test]
    fn gate_caught_up_is_ok() {
        let g = evaluate_lag_gate(Some(&replica("replica", Some(0))), MAX_SAFE_LAG_BYTES);
        assert_eq!(g, LagGate::Ok { lag_bytes: 0 });
        assert!(g.is_safe());
    }

    #[test]
    fn gate_at_threshold_is_ok() {
        let g = evaluate_lag_gate(
            Some(&replica("replica", Some(MAX_SAFE_LAG_BYTES))),
            MAX_SAFE_LAG_BYTES,
        );
        assert!(g.is_safe());
    }

    #[test]
    fn gate_one_over_threshold_blocks() {
        let g = evaluate_lag_gate(
            Some(&replica("replica", Some(MAX_SAFE_LAG_BYTES + 1))),
            MAX_SAFE_LAG_BYTES,
        );
        assert_eq!(
            g,
            LagGate::LagTooHigh {
                lag_bytes: MAX_SAFE_LAG_BYTES + 1,
                max: MAX_SAFE_LAG_BYTES
            }
        );
        assert!(!g.is_safe());
    }

    #[test]
    fn gate_null_lag_is_unknown_not_ok() {
        let g = evaluate_lag_gate(Some(&replica("replica", None)), MAX_SAFE_LAG_BYTES);
        assert_eq!(g, LagGate::LagUnknown);
        assert!(!g.is_safe());
    }

    #[test]
    fn gate_wrong_role_blocks() {
        let g = evaluate_lag_gate(Some(&replica("primary", Some(0))), MAX_SAFE_LAG_BYTES);
        assert_eq!(
            g,
            LagGate::WrongRole {
                role: "primary".to_string()
            }
        );
        assert!(!g.is_safe());
    }

    // ── dry-run plan builder ───────────────────────────────────────────────
    fn inputs_with(gate: LagGate) -> PlanInputs {
        PlanInputs {
            target_member: "james".to_string(),
            target_ip: "192.168.5.108".to_string(),
            current_primary_member: Some("taylor".to_string()),
            current_leader_member: Some("taylor".to_string()),
            new_dsn: "postgres://forgefleet:secret@192.168.5.108:55432/forgefleet".to_string(),
            lease_minutes: 30,
            lag_gate: gate,
        }
    }

    #[test]
    fn plan_safe_when_gate_ok() {
        let plan = build_plan(&inputs_with(LagGate::Ok { lag_bytes: 0 }));
        assert!(plan.safe);
        assert!(plan.blocking_reason.is_none());
        // §4 ordering: exactly 4 steps, numbered 1..=4.
        assert_eq!(plan.steps.len(), 4);
        let orders: Vec<u8> = plan.steps.iter().map(|s| s.order).collect();
        assert_eq!(orders, vec![1, 2, 3, 4]);
        // step 1 is a pure check, steps 2..4 mutate.
        assert!(!plan.steps[0].mutates);
        assert!(plan.steps[1..].iter().all(|s| s.mutates));
    }

    #[test]
    fn plan_unsafe_carries_blocking_reason_but_still_renders() {
        let gate = LagGate::LagTooHigh {
            lag_bytes: 9_000_000,
            max: MAX_SAFE_LAG_BYTES,
        };
        let plan = build_plan(&inputs_with(gate));
        assert!(!plan.safe);
        assert!(plan.blocking_reason.is_some());
        // Still a full 4-step plan so the operator sees the whole ordering.
        assert_eq!(plan.steps.len(), 4);
    }

    #[test]
    fn plan_redacts_dsn_password() {
        // Use a distinctive password so the assertion can't collide with the
        // literal word "secret" in "fleet_secret" inside the step text.
        let mut inp = inputs_with(LagGate::Ok { lag_bytes: 0 });
        inp.new_dsn = "postgres://forgefleet:hunter2pw@10.0.0.9:55432/forgefleet".to_string();
        let plan = build_plan(&inp);
        let repoint = &plan.steps[2].detail;
        assert!(repoint.contains("***"));
        assert!(!repoint.contains("hunter2pw"));
    }

    #[test]
    fn redact_dsn_handles_passwordless_and_plain() {
        assert_eq!(
            redact_dsn("postgres://user:pw@host:55432/db"),
            "postgres://user:***@host:55432/db"
        );
        // No password segment — left as-is.
        assert_eq!(
            redact_dsn("postgres://user@host/db"),
            "postgres://user@host/db"
        );
        // Not a URL — returned unchanged.
        assert_eq!(redact_dsn("not-a-dsn"), "not-a-dsn");
    }
}
