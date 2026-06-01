//! `ff reserve` + `ff arbiter <sub>` — CLI for the V119 global resource arbiter
//! (backlog #7). EXPLICIT-declaration: a session/operator declares an intent to
//! reserve a host SET for a span; the leader-gated `ff_agent::arbiter` tick
//! grants it all-or-nothing under the `fleet_secrets.arbiter_mode` gate (DEFAULT
//! OFF — nothing actuates unless an operator opts in to `active`).
//!
//! All handlers open the pool via `ff_agent::fleet_info::get_fleet_pool()`
//! exactly like the Fabric / Tasks arms.

use anyhow::{Result, anyhow};
use ff_agent::arbiter::{self, ArbiterMode};

fn pool_err(e: String) -> anyhow::Error {
    anyhow!("connect Postgres: {e}")
}

/// `ff reserve --hosts <list|dgx-pair:a-b> --for <dur> --task <desc> ...`
/// Inserts a `work_intents` row (state=pending) and prints the planned
/// grant/prework/queue/restore. In dry-run/off it actuates nothing — the tick
/// (or `ff arbiter grant`) does the work when the gate allows.
#[allow(clippy::too_many_arguments)]
pub async fn handle_reserve(
    hosts_spec: &str,
    for_dur: &str,
    task: &str,
    exclusive: bool,
    priority: i64,
    project: Option<String>,
) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(pool_err)?;

    let hosts = arbiter::sorted_host_set(&arbiter::expand_hosts(hosts_spec));
    if hosts.is_empty() {
        return Err(anyhow!("no hosts parsed from --hosts '{hosts_spec}'"));
    }
    let requested_secs = parse_duration_secs(for_dur)
        .ok_or_else(|| anyhow!("could not parse --for '{for_dur}' (try 2h, 30m, 3600s)"))?;

    let requester = ff_agent::fleet_info::resolve_this_worker_name().await;
    let host_set_json = serde_json::Value::Array(
        hosts
            .iter()
            .map(|h| serde_json::Value::String(h.clone()))
            .collect(),
    );
    let (prework, restore) = if exclusive {
        arbiter::default_plans(&hosts)
    } else {
        (serde_json::json!([]), serde_json::json!([]))
    };

    let intent_id = ff_db::pg_insert_work_intent(
        &pool,
        &requester,
        project.as_deref(),
        &host_set_json,
        &serde_json::json!([]),
        exclusive,
        requested_secs,
        priority,
        Some(task),
        &prework,
        &restore,
    )
    .await
    .map_err(|e| anyhow!("insert work_intent: {e}"))?;

    let mode = arbiter::read_mode(&pool).await;
    println!("reserved intent {intent_id} (state=pending)");
    println!(
        "  arbiter_mode = {} (default off ⇒ no actuation)",
        mode.as_str()
    );

    if let Some(intent) = ff_db::pg_get_work_intent(&pool, &intent_id)
        .await
        .map_err(|e| anyhow!("get work_intent: {e}"))?
    {
        print!("{}", arbiter::render_plan(&intent));
    }
    if mode == ArbiterMode::Off {
        println!(
            "  NOTE: gate is OFF — the intent is queued but the arbiter tick will not act on it."
        );
        println!(
            "        Set `ff secrets set arbiter_mode dry-run` (plan-only) or `active` to enable."
        );
    }
    Ok(())
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum ArbiterCommand {
    /// Show the arbiter mode, current leader, all reserved hosts (owner +
    /// expiry), and the pending FIFO queue.
    Status,
    /// List work_intents (deterministic ORDER BY priority DESC, created_at ASC).
    List {
        /// Include terminal (done/denied) intents too.
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Operator force-attempt the set-atomic grant for one pending intent
    /// (still gated: no-op unless arbiter_mode=active; dry-run prints the plan).
    Grant {
        /// work_intents.id (UUID).
        intent_id: String,
    },
    /// Transition active→releasing, run the restore plan, free the host set
    /// (idempotent; safe if already released).
    Release {
        /// work_intents.id (UUID).
        intent_id: String,
    },
}

pub async fn handle_arbiter(command: ArbiterCommand) -> Result<()> {
    let pool = ff_agent::fleet_info::get_fleet_pool()
        .await
        .map_err(pool_err)?;

    match command {
        ArbiterCommand::Status => {
            let mode = arbiter::read_mode(&pool).await;
            println!("arbiter_mode: {}", mode.as_str());

            let leader: Option<String> = sqlx::query_scalar(
                "SELECT member_name FROM fleet_leader_state \
                 WHERE heartbeat_at > NOW() - INTERVAL '60 seconds' \
                 ORDER BY heartbeat_at DESC LIMIT 1",
            )
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
            println!("leader: {}", leader.as_deref().unwrap_or("(none)"));

            let reserved = ff_db::pg_list_reserved_hosts(&pool)
                .await
                .map_err(|e| anyhow!("list reserved hosts: {e}"))?;
            println!("\nreserved hosts ({}):", reserved.len());
            for h in &reserved {
                println!(
                    "  {:<12} state={:<9} owner={:<38} expires={} reason={}",
                    h.name,
                    h.reservation_state,
                    h.reservation_owner.as_deref().unwrap_or("-"),
                    h.reservation_expires_at
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "-".to_string()),
                    h.reserved_reason.as_deref().unwrap_or("-"),
                );
            }

            let pending = ff_db::pg_pending_work_intents(&pool)
                .await
                .map_err(|e| anyhow!("pending intents: {e}"))?;
            println!("\npending FIFO queue ({}):", pending.len());
            for i in &pending {
                println!(
                    "  {} prio={} requester={} hosts={} task={}",
                    i.id,
                    i.priority,
                    i.requester,
                    serde_json::to_string(&i.target_host_set).unwrap_or_default(),
                    i.task_desc.as_deref().unwrap_or("-"),
                );
            }
            Ok(())
        }
        ArbiterCommand::List { all } => {
            let rows = ff_db::pg_list_work_intents(&pool, !all)
                .await
                .map_err(|e| anyhow!("list intents: {e}"))?;
            println!("{} work_intents:", rows.len());
            for i in &rows {
                println!(
                    "  {} state={:<9} prio={:<4} requester={:<14} hosts={} expires={} task={}",
                    i.id,
                    i.state,
                    i.priority,
                    i.requester,
                    serde_json::to_string(&i.target_host_set).unwrap_or_default(),
                    i.expires_at
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "-".to_string()),
                    i.task_desc.as_deref().unwrap_or("-"),
                );
            }
            Ok(())
        }
        ArbiterCommand::Grant { intent_id } => {
            let intent = ff_db::pg_get_work_intent(&pool, &intent_id)
                .await
                .map_err(|e| anyhow!("get intent: {e}"))?
                .ok_or_else(|| anyhow!("intent {intent_id} not found"))?;
            if intent.state != "pending" {
                return Err(anyhow!(
                    "intent {intent_id} is in state '{}' (only 'pending' can be granted)",
                    intent.state
                ));
            }
            let mode = arbiter::read_mode(&pool).await;
            let hosts = arbiter::sorted_host_set(&arbiter::host_set_of(&intent));
            print!("{}", arbiter::render_plan(&intent));

            match mode {
                ArbiterMode::Off => {
                    println!("arbiter_mode=off — no-op. Plan shown above only.");
                    Ok(())
                }
                ArbiterMode::DryRun => {
                    println!("arbiter_mode=dry-run — plan shown above; actuating nothing.");
                    Ok(())
                }
                ArbiterMode::Active => {
                    let won = ff_db::pg_arbiter_grant_set(
                        &pool,
                        &intent.id,
                        &hosts,
                        intent.requested_secs,
                    )
                    .await
                    .map_err(|e| anyhow!("grant set: {e}"))?;
                    if won {
                        ff_db::pg_set_work_intent_state(&pool, &intent.id, "granted", None)
                            .await
                            .map_err(|e| anyhow!("set granted: {e}"))?;
                        println!("granted set [{}] to intent {}", hosts.join(", "), intent.id);
                    } else {
                        println!(
                            "could not grant: at least one host in [{}] is not available; intent stays pending",
                            hosts.join(", ")
                        );
                    }
                    Ok(())
                }
            }
        }
        ArbiterCommand::Release { intent_id } => {
            let intent = ff_db::pg_get_work_intent(&pool, &intent_id)
                .await
                .map_err(|e| anyhow!("get intent: {e}"))?
                .ok_or_else(|| anyhow!("intent {intent_id} not found"))?;
            let mode = arbiter::read_mode(&pool).await;
            arbiter::release_intent(&pool, &intent, mode).await;
            match mode {
                ArbiterMode::Active => {
                    println!("released intent {intent_id} (restore ran, hosts freed)")
                }
                _ => println!(
                    "arbiter_mode={} — release plan shown in logs; actuated nothing",
                    mode.as_str()
                ),
            }
            Ok(())
        }
    }
}

/// Parse a duration like "2h", "30m", "45s", "3600" (bare = seconds).
fn parse_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else {
        (s, 1)
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|v| (v * mult as f64) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration_secs("2h"), Some(7200));
        assert_eq!(parse_duration_secs("30m"), Some(1800));
        assert_eq!(parse_duration_secs("45s"), Some(45));
        assert_eq!(parse_duration_secs("3600"), Some(3600));
        assert_eq!(parse_duration_secs(""), None);
        assert_eq!(parse_duration_secs("xyz"), None);
    }
}
