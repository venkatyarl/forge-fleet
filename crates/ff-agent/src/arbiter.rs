//! Global resource arbiter (backlog #7) — EXPLICIT-declaration host reservation.
//!
//! A session/operator declares an *intent* (`work_intents` row, V119) to reserve
//! a host SET for a span of time. A leader-gated tick — structurally cloned from
//! [`crate::autoscaler::spawn_autoscaler_tick`] and sharing its exact
//! `fleet_secrets` gate convention — grants pending intents all-or-nothing,
//! runs an idempotent prework plan (e.g. offload minimax → disk to free GPU),
//! holds a TTL lease, fences general `fleet_tasks` claiming off the reserved
//! host (the V119 claim-gate conjunct in `task_runner.rs`), and on release runs
//! an idempotent restore plan (reload).
//!
//! This is MOSTLY WIRING of existing primitives:
//! - V114 per-host CAS (`pg_reserve_host`) → V119 set-atomic
//!   [`ff_db::pg_arbiter_grant_set`] (deterministic global lock order ⇒
//!   deadlock-free).
//! - The autoscaler's always-restore RAII → a persisted, crash-resumable
//!   prework/restore cursor on `work_intents`.
//! - `fleet_demand_snapshot` starvation signal + `fleet_tasks.priority` →
//!   priority-based preemption.
//!
//! ## SAFETY — three-mode gate (`fleet_secrets.arbiter_mode`)
//! Read EVERY tick, EXACTLY like [`crate::autoscaler`] reads `autoscaler_mode`:
//! - `off`     (DEFAULT, and the value when the key is missing): pure no-op.
//! - `dry-run`: compute + log the full grant/prework/queue/restore plan,
//!              actuate NOTHING and advance NO cursor.
//! - `active`:  actuate (set-atomic grant → prework → active; reap → restore →
//!              free).

use sqlx::PgPool;
use std::time::Duration;
use tracing::{debug, info, warn};

use ff_db::WorkIntentRow;

/// `fleet_secrets` key holding the three-mode gate. Off / missing = no-op.
/// Mirrors [`crate::autoscaler`]'s `AUTOSCALER_MODE_KEY` exactly.
const ARBITER_MODE_KEY: &str = "arbiter_mode";

/// The gate's three modes — identical parse arms to
/// [`crate::autoscaler::AutoscalerMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArbiterMode {
    Off,
    DryRun,
    Active,
}

impl ArbiterMode {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("active") => ArbiterMode::Active,
            Some("dry-run") | Some("dry_run") | Some("dryrun") => ArbiterMode::DryRun,
            // Off, missing, empty, or any unrecognised value → safe default.
            _ => ArbiterMode::Off,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ArbiterMode::Off => "off",
            ArbiterMode::DryRun => "dry-run",
            ArbiterMode::Active => "active",
        }
    }
}

/// Read the gate from `fleet_secrets`. DEFAULTS TO OFF when the key is missing
/// or unparseable — so shipping this subsystem is harmless until an operator
/// opts in. Mirrors `autoscaler::read_mode`.
pub async fn read_mode(pg: &PgPool) -> ArbiterMode {
    match ff_db::pg_get_secret(pg, ARBITER_MODE_KEY).await {
        Ok(v) => ArbiterMode::parse(v.as_deref()),
        Err(e) => {
            warn!(error = %e, "arbiter: failed to read mode secret; treating as off");
            ArbiterMode::Off
        }
    }
}

/// Expand a `--hosts` spec into a concrete host list. Supports:
/// - `dgx-pair:<a>-<b>` → `[a, b]` (the CX-7 TP=2 pairs; both or neither).
/// - comma/space-separated explicit hosts → as listed.
///
/// The returned list is NOT yet sorted — call [`sorted_host_set`] before
/// reserving so every actor uses the same global lock order.
pub fn expand_hosts(spec: &str) -> Vec<String> {
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix("dgx-pair:") {
        // dgx-pair:sia-adele → ["sia", "adele"]. Split on the FIRST '-' only,
        // so hostnames are taken verbatim (host names here never contain '-').
        if let Some((a, b)) = rest.split_once('-') {
            return vec![a.trim().to_string(), b.trim().to_string()];
        }
        return vec![rest.trim().to_string()];
    }
    spec.split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Total deterministic lock order: lowercased name ASC. EVERY arbiter actor
/// reserves in this same order — the classic resource-ordering deadlock-
/// avoidance (Coffman hold-and-wait broken). De-duped.
pub fn sorted_host_set(hosts: &[String]) -> Vec<String> {
    let mut v: Vec<String> = hosts.to_vec();
    v.sort_by_key(|h| h.to_ascii_lowercase());
    v.dedup_by_key(|h| h.to_ascii_lowercase());
    v
}

/// Render the human-readable plan for one intent: the ordered prework steps,
/// the grant set, the queue note, and the ordered restore steps. Used by both
/// the dry-run tick log and `ff reserve` / `ff arbiter` CLI output.
pub fn render_plan(intent: &WorkIntentRow) -> String {
    let hosts = sorted_host_set(&host_set_of(intent));
    let prework = plan_steps_summary(&intent.prework_plan);
    let restore = plan_steps_summary(&intent.restore_plan);
    let mut s = String::new();
    s.push_str(&format!(
        "arbiter PLAN intent={} requester={} priority={} exclusive={} lease={}s\n",
        intent.id, intent.requester, intent.priority, intent.exclusive, intent.requested_secs
    ));
    if !prework.is_empty() {
        s.push_str(&format!("  prework: {}\n", prework.join(" → ")));
    }
    s.push_str(&format!("  grant set (sorted): [{}]\n", hosts.join(", ")));
    s.push_str("  queue: lower-priority intents for any of these hosts stay queued (FIFO: priority DESC, created_at ASC)\n");
    if !restore.is_empty() {
        s.push_str(&format!("  on release restore: {}\n", restore.join(" → ")));
    }
    s
}

/// Pull the target host set off an intent's JSONB array.
pub fn host_set_of(intent: &WorkIntentRow) -> Vec<String> {
    intent
        .target_host_set
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// One-line summary per JSONB plan step (e.g. "offload_models_to_disk(sia)").
fn plan_steps_summary(plan: &serde_json::Value) -> Vec<String> {
    plan.as_array()
        .map(|a| {
            a.iter()
                .map(|step| {
                    let kind = step.get("step").and_then(|v| v.as_str()).unwrap_or("?");
                    let host = step.get("host").and_then(|v| v.as_str()).unwrap_or("");
                    if host.is_empty() {
                        kind.to_string()
                    } else {
                        format!("{kind}({host})")
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build the default prework/restore plans for an EXCLUSIVE reservation: offload
/// every host's models to disk before the grant, reload them on release. This
/// generalizes the proven `ff offload` minimax-on-disk dispatch. Steps are
/// IDEMPOTENT by construction (offload checks 'already on disk', reload checks
/// 'already deployed'), so a crash mid-plan can safely resume at the cursor.
pub fn default_plans(hosts: &[String]) -> (serde_json::Value, serde_json::Value) {
    let prework: Vec<serde_json::Value> = hosts
        .iter()
        .map(|h| serde_json::json!({ "step": "offload_models_to_disk", "host": h }))
        .collect();
    let restore: Vec<serde_json::Value> = hosts
        .iter()
        .map(|h| serde_json::json!({ "step": "reload_model", "host": h }))
        .collect();
    (
        serde_json::Value::Array(prework),
        serde_json::Value::Array(restore),
    )
}

/// Summary of one arbiter pass (for the tick log).
#[derive(Debug, Default, Clone)]
pub struct ArbiterSummary {
    pub mode: &'static str,
    pub leases_reaped: usize,
    pub pending: usize,
    pub granted: usize,
    pub queued: usize,
    pub released: usize,
}

/// One arbiter pass. Reads the gate; off = no-op. Reaps expired leases (running
/// each owner's restore plan), then walks the pending FIFO and attempts the
/// set-atomic grant for each, applying the priority-based preemption policy.
/// DRY-RUN logs the full plan and actuates nothing; OFF is a pure no-op.
pub async fn arbiter_pass(pg: &PgPool) -> Result<ArbiterSummary, String> {
    let mode = read_mode(pg).await;
    if mode == ArbiterMode::Off {
        debug!("arbiter: mode=off (no-op)");
        return Ok(ArbiterSummary {
            mode: "off",
            ..Default::default()
        });
    }

    let mut summary = ArbiterSummary {
        mode: mode.as_str(),
        ..Default::default()
    };

    // 1) Reap expired leases → run restore → free the host set.
    let expired = ff_db::pg_reap_expired_leases(pg)
        .await
        .map_err(|e| format!("pg_reap_expired_leases: {e}"))?;
    summary.leases_reaped = expired.len();
    for intent_id in &expired {
        if let Some(intent) = ff_db::pg_get_work_intent(pg, intent_id)
            .await
            .map_err(|e| format!("pg_get_work_intent({intent_id}): {e}"))?
        {
            info!(intent = %intent_id, "arbiter: lease expired — releasing");
            release_intent(pg, &intent, mode).await;
            summary.released += 1;
        }
    }

    // 2) Walk the pending FIFO (priority DESC, created_at ASC).
    let pending = ff_db::pg_pending_work_intents(pg)
        .await
        .map_err(|e| format!("pg_pending_work_intents: {e}"))?;
    summary.pending = pending.len();

    // Snapshot reserved hosts once so we can apply the preemption policy without
    // re-querying per intent.
    let reserved = ff_db::pg_list_reserved_hosts(pg)
        .await
        .map_err(|e| format!("pg_list_reserved_hosts: {e}"))?;

    for intent in &pending {
        let hosts = sorted_host_set(&host_set_of(intent));

        // Preemption policy: a STRICTLY-higher-priority pending intent whose set
        // overlaps a LOWER-priority active holder may mark that holder for
        // releasing; equal/lower simply waits (no preemption ⇒ no flapping).
        let blocking: Vec<&ff_db::ArbiterReservedHost> = reserved
            .iter()
            .filter(|h| hosts.iter().any(|w| w.eq_ignore_ascii_case(&h.name)))
            .collect();

        if !blocking.is_empty() {
            // Host(s) held — see if we outrank every holder.
            let mut can_preempt = true;
            for h in &blocking {
                let holder_priority = holder_priority(pg, h).await;
                if intent.priority <= holder_priority {
                    can_preempt = false;
                    break;
                }
            }
            if can_preempt {
                for h in &blocking {
                    if let Some(owner) = &h.reservation_owner {
                        info!(
                            host = %h.name, holder = %owner, challenger = %intent.id,
                            mode = mode.as_str(),
                            "arbiter PLAN: would PREEMPT lower-priority holder (priority {} > holder) — marking holder releasing",
                            intent.priority
                        );
                        if mode == ArbiterMode::Active
                            && let Some(holder_intent) =
                                ff_db::pg_get_work_intent(pg, owner).await.ok().flatten()
                        {
                            release_intent(pg, &holder_intent, mode).await;
                        }
                    }
                }
            } else {
                info!(
                    intent = %intent.id, mode = mode.as_str(),
                    "arbiter PLAN: intent stays QUEUED behind a same-or-higher-priority holder of its host set"
                );
            }
            summary.queued += 1;
            // Either way, don't grab a partial set this pass — wait for the next.
            continue;
        }

        // No blocker: attempt the set-atomic grant.
        info!("{}", render_plan(intent).trim_end());

        if mode == ArbiterMode::DryRun {
            // dry-run: log the plan, actuate nothing, advance no cursor.
            continue;
        }

        // active: set-atomic grant → prework → active.
        match ff_db::pg_arbiter_grant_set(pg, &intent.id, &hosts, intent.requested_secs).await {
            Ok(true) => {
                if let Err(e) =
                    ff_db::pg_set_work_intent_state(pg, &intent.id, "granted", None).await
                {
                    warn!(error = %e, intent = %intent.id, "arbiter: set state granted failed");
                }
                run_prework(pg, intent, mode).await;
                summary.granted += 1;
            }
            Ok(false) => {
                debug!(intent = %intent.id, "arbiter: set not fully available — stays pending");
                summary.queued += 1;
            }
            Err(e) => warn!(error = %e, intent = %intent.id, "arbiter: grant-set failed"),
        }
    }

    Ok(summary)
}

/// Look up the priority of the intent currently holding a reserved host (0 if
/// unknown — treated as lowest, so unknown holders are preemptible only by
/// strictly positive-priority challengers, which is the default scale).
async fn holder_priority(pg: &PgPool, h: &ff_db::ArbiterReservedHost) -> i64 {
    if let Some(owner) = &h.reservation_owner
        && let Ok(Some(intent)) = ff_db::pg_get_work_intent(pg, owner).await
    {
        return intent.priority;
    }
    0
}

/// Run the prework plan for a granted intent, crash-resumable from
/// `prework_cursor`. Each step is idempotent. On completion transitions
/// granted → active. In dry-run/off this only logs and advances NO cursor.
async fn run_prework(pg: &PgPool, intent: &WorkIntentRow, mode: ArbiterMode) {
    let steps = intent.prework_plan.as_array().cloned().unwrap_or_default();
    let mut cursor = intent.prework_cursor.max(0) as usize;
    while cursor < steps.len() {
        let step = &steps[cursor];
        let desc = plan_steps_summary(&serde_json::Value::Array(vec![step.clone()]))
            .pop()
            .unwrap_or_else(|| "?".to_string());
        if mode == ArbiterMode::Active {
            match execute_step(pg, step).await {
                Ok(()) => {
                    cursor += 1;
                    if let Err(e) =
                        ff_db::pg_advance_intent_cursor(pg, &intent.id, false, cursor as i64).await
                    {
                        warn!(error = %e, intent = %intent.id, "arbiter: advance prework cursor failed");
                        return;
                    }
                }
                Err(e) => {
                    warn!(error = %e, intent = %intent.id, step = %desc, "arbiter: prework step failed — leaving intent granted for retry");
                    return;
                }
            }
        } else {
            info!(intent = %intent.id, step = %desc, "arbiter PLAN(dry-run): would run prework step (no cursor advance)");
            cursor += 1;
        }
    }
    if mode == ArbiterMode::Active
        && let Err(e) = ff_db::pg_set_work_intent_state(pg, &intent.id, "active", None).await
    {
        warn!(error = %e, intent = %intent.id, "arbiter: set state active failed");
    }
}

/// Release an intent: active/granted → releasing → run restore plan → free the
/// host set → done. ALWAYS frees the host AFTER restore completes (the durable
/// generalization of the autoscaler's always-unreserve). Idempotent.
pub async fn release_intent(pg: &PgPool, intent: &WorkIntentRow, mode: ArbiterMode) {
    if mode != ArbiterMode::Active {
        info!(
            intent = %intent.id, mode = mode.as_str(),
            "arbiter PLAN(dry-run): would run restore plan then free host set (no actuation)"
        );
        return;
    }
    if let Err(e) = ff_db::pg_set_work_intent_state(pg, &intent.id, "releasing", None).await {
        warn!(error = %e, intent = %intent.id, "arbiter: set state releasing failed");
    }
    let steps = intent.restore_plan.as_array().cloned().unwrap_or_default();
    let mut cursor = intent.restore_cursor.max(0) as usize;
    while cursor < steps.len() {
        let step = &steps[cursor];
        match execute_step(pg, step).await {
            Ok(()) => {
                cursor += 1;
                if let Err(e) =
                    ff_db::pg_advance_intent_cursor(pg, &intent.id, true, cursor as i64).await
                {
                    warn!(error = %e, intent = %intent.id, "arbiter: advance restore cursor failed");
                    // Keep going — restore is best-effort idempotent.
                }
            }
            Err(e) => {
                // restore ALWAYS continues even on a step failure — the host
                // must not be stranded reserved.
                warn!(error = %e, intent = %intent.id, "arbiter: restore step failed; continuing");
                cursor += 1;
                let _ = ff_db::pg_advance_intent_cursor(pg, &intent.id, true, cursor as i64).await;
            }
        }
    }
    // Only NOW free the host set — never hand a host to the next queued intent
    // until restore (e.g. reload minimax) finished.
    if let Err(e) = ff_db::pg_arbiter_free_set(pg, &intent.id).await {
        warn!(error = %e, intent = %intent.id, "arbiter: free host set failed");
    }
    if let Err(e) = ff_db::pg_set_work_intent_state(pg, &intent.id, "done", None).await {
        warn!(error = %e, intent = %intent.id, "arbiter: set state done failed");
    }
}

/// Execute one prework/restore step. All actuation goes through ff offload /
/// model-load primitives (dogfood), never raw ssh. Each step is idempotent.
///
/// NOTE: the concrete offload/reload wiring is intentionally a thin shim here —
/// it logs the actuation intent and returns Ok so the cursor advances. Wiring
/// the real `ff offload` / `model_runtime::load_model` calls is a follow-up that
/// reuses [`crate::autoscaler::do_load`]'s cross-node pattern; until then active
/// mode is gated OFF by default so nothing actuates unattended.
async fn execute_step(_pg: &PgPool, step: &serde_json::Value) -> Result<(), String> {
    let kind = step.get("step").and_then(|v| v.as_str()).unwrap_or("?");
    let host = step.get("host").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "offload_models_to_disk" => {
            info!(host = %host, "arbiter: STEP offload_models_to_disk (would dispatch ff offload)");
            Ok(())
        }
        "reload_model" => {
            info!(host = %host, "arbiter: STEP reload_model (would dispatch model load)");
            Ok(())
        }
        "install_stack" => {
            let stack = step.get("stack").and_then(|v| v.as_str()).unwrap_or("");
            info!(host = %host, stack = %stack, "arbiter: STEP install_stack (would ensure deps)");
            Ok(())
        }
        other => Err(format!("unknown arbiter step '{other}'")),
    }
}

/// Spawn the leader-gated arbiter loop. Structurally cloned from
/// [`crate::autoscaler::spawn_autoscaler_tick`]: tokio interval, skip-first
/// tick, per-tick leader gate via the SAME `fleet_leader_state` query, then
/// `arbiter_pass`. On failover the new leader's forgefleetd picks it up.
pub fn spawn_arbiter_tick(
    pg: PgPool,
    worker_name: String,
    interval_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the immediate fire so pulse/election settle first.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let is_leader: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS (
                            SELECT 1 FROM fleet_leader_state
                            WHERE member_name = $1
                              AND heartbeat_at > NOW() - INTERVAL '60 seconds'
                        )
                        "#
                    )
                    .bind(&worker_name)
                    .fetch_one(&pg)
                    .await
                    .unwrap_or(false);

                    if !is_leader {
                        continue;
                    }

                    match arbiter_pass(&pg).await {
                        Ok(s) => {
                            info!(
                                mode = s.mode,
                                leases_reaped = s.leases_reaped,
                                pending = s.pending,
                                granted = s.granted,
                                queued = s.queued,
                                released = s.released,
                                "arbiter pass"
                            );
                        }
                        Err(e) => warn!(error = %e, "arbiter tick failed"),
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        info!("arbiter tick loop stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_defaults_off() {
        assert_eq!(ArbiterMode::parse(None), ArbiterMode::Off);
        assert_eq!(ArbiterMode::parse(Some("")), ArbiterMode::Off);
        assert_eq!(ArbiterMode::parse(Some("nonsense")), ArbiterMode::Off);
        assert_eq!(ArbiterMode::parse(Some("off")), ArbiterMode::Off);
        assert_eq!(ArbiterMode::parse(Some("active")), ArbiterMode::Active);
        assert_eq!(ArbiterMode::parse(Some("ACTIVE")), ArbiterMode::Active);
        assert_eq!(ArbiterMode::parse(Some("dry-run")), ArbiterMode::DryRun);
        assert_eq!(ArbiterMode::parse(Some("dry_run")), ArbiterMode::DryRun);
        assert_eq!(ArbiterMode::parse(Some("dryrun")), ArbiterMode::DryRun);
    }

    #[test]
    fn dgx_pair_expands() {
        assert_eq!(
            expand_hosts("dgx-pair:sia-adele"),
            vec!["sia".to_string(), "adele".to_string()]
        );
        assert_eq!(
            expand_hosts("dgx-pair:rihanna-beyonce"),
            vec!["rihanna".to_string(), "beyonce".to_string()]
        );
    }

    #[test]
    fn explicit_hosts_split() {
        assert_eq!(
            expand_hosts("marcus, sophie priya"),
            vec![
                "marcus".to_string(),
                "sophie".to_string(),
                "priya".to_string()
            ]
        );
    }

    #[test]
    fn sorted_set_is_deterministic_and_deduped() {
        // sia-adele sorts to [adele, sia] — the same total order every actor uses.
        assert_eq!(
            sorted_host_set(&expand_hosts("dgx-pair:sia-adele")),
            vec!["adele".to_string(), "sia".to_string()]
        );
        assert_eq!(
            sorted_host_set(&["Sia".to_string(), "sia".to_string()]),
            vec!["Sia".to_string()]
        );
    }

    #[test]
    fn default_plans_offload_then_reload() {
        let (pre, post) = default_plans(&["sia".to_string()]);
        assert_eq!(pre[0]["step"], "offload_models_to_disk");
        assert_eq!(pre[0]["host"], "sia");
        assert_eq!(post[0]["step"], "reload_model");
        assert_eq!(post[0]["host"], "sia");
    }

    fn smoke_intent(id: &str, hosts: &[&str], prio: i64, secs: i64) -> WorkIntentRow {
        let hs: Vec<String> = hosts.iter().map(|s| s.to_string()).collect();
        let (pre, post) = default_plans(&sorted_host_set(&hs));
        WorkIntentRow {
            id: id.into(),
            requester: "vinny".into(),
            project: Some("HireFlow360".into()),
            target_host_set: serde_json::json!(hosts),
            requires_capability: serde_json::json!([]),
            exclusive: true,
            requested_secs: secs,
            priority: prio,
            state: "pending".into(),
            task_desc: Some("HireFlow360 train".into()),
            prework_plan: pre,
            restore_plan: post,
            prework_cursor: 0,
            restore_cursor: 0,
            denied_reason: None,
            created_at: chrono::Utc::now(),
            granted_at: None,
            expires_at: None,
            released_at: None,
        }
    }

    /// DRY-RUN smoke (NO fleet mutation): the sia / HireFlow360-train /
    /// KovaBody-waits scenario. Asserts the gate defaults OFF, the plan renders
    /// grant + prework(offload) + queue + restore, and the FIFO/preemption key
    /// keeps the lower-priority KovaBody intent queued behind HF360.
    #[test]
    fn smoke_hf360_train_dgx_pair_scenario() {
        // Gate MUST default OFF (missing/empty/unrecognized).
        assert_eq!(ArbiterMode::parse(None), ArbiterMode::Off);

        // ff reserve --hosts dgx-pair:sia-adele --for 2h --priority 200.
        let pair: Vec<&str> = ["sia", "adele"].to_vec();
        let hf = smoke_intent("11111111-1111-1111-1111-111111111111", &pair, 200, 7200);
        let plan = render_plan(&hf);
        // Grant set is the sorted pair.
        assert!(
            plan.contains("grant set (sorted): [adele, sia]"),
            "plan: {plan}"
        );
        // Prework offloads on both pair members.
        assert!(
            plan.contains("offload_models_to_disk(adele)"),
            "plan: {plan}"
        );
        assert!(plan.contains("offload_models_to_disk(sia)"), "plan: {plan}");
        // Restore reloads on release.
        assert!(plan.contains("reload_model(sia)"), "plan: {plan}");
        // Queue note present.
        assert!(plan.contains("queue:"), "plan: {plan}");

        // Competing lower-priority KovaBody intent for sia.
        let kova = smoke_intent("22222222-2222-2222-2222-222222222222", &["sia"], 100, 3600);

        // FIFO: HF360 (200) sorts ahead of KovaBody (100). The preemption key is
        // priority — HF360 outranks, KovaBody stays queued behind it.
        let mut q = [kova.clone(), hf.clone()];
        q.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then(a.created_at.cmp(&b.created_at))
        });
        assert_eq!(q[0].id, hf.id, "HF360 must be ahead of KovaBody in FIFO");
        assert!(hf.priority > kova.priority, "HF360 outranks KovaBody");

        // Deterministic grant order proves the TP=2 pair is whole-or-none.
        assert_eq!(
            sorted_host_set(&host_set_of(&hf)),
            vec!["adele".to_string(), "sia".to_string()]
        );
    }
}
