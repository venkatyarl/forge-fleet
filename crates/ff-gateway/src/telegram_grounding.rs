//! Grounded Telegram answers (roadmap E1).
//!
//! The Telegram conversation used to be owned by an UNGROUNDED LLM agent
//! (OpenClaw, with its `nodes`/`gateway` tools stripped) that hallucinated
//! fake computer names, fake work items, and shifting ETAs whenever the
//! operator asked "show me my work items" or "what computers are you using".
//!
//! This module intercepts those two fleet/PM questions BEFORE they reach the
//! free-text LLM path ([`super::telegram_transport`]'s `handle_brain_message`)
//! and answers them from REAL Postgres state (`work_items`, `fleet_workers`).
//! Anything else returns [`TelegramIntent::General`] and falls through to the
//! LLM as before.
//!
//! The classifier is pure → unit-tested with the operator's actual phrasings.

use anyhow::Result;
use sqlx::{PgPool, Row};

/// What an inbound Telegram message is asking for, as far as grounded answers
/// are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramIntent {
    /// "what are you working on" / "items planned" / "progress" / "eta".
    WorkStatus,
    /// "which computers" / "fleet" / "nodes" / "machines".
    FleetRoster,
    /// Anything else — defer to the LLM chat path.
    General,
}

/// Classify an inbound message. Pure.
///
/// Work-status intent wins when BOTH work and fleet keywords appear (the
/// operator's "show me all items you are working on … using all the computers"
/// is primarily a work-status question), so the work check is first. A bare
/// "which computers are being used" with no work keyword → `FleetRoster`.
pub fn classify_intent(text: &str) -> TelegramIntent {
    let t = text.to_ascii_lowercase();

    // Work / PM status.
    const WORK_PHRASES: &[&str] = &[
        "working on",
        "work item",
        "items you",
        "items planned",
        "planned next",
        "what's planned",
        "whats planned",
        "roadmap",
        "progress",
        "eta",
        "building",
        "what are you doing",
        "what are u doing",
    ];
    if WORK_PHRASES.iter().any(|p| t.contains(p)) {
        return TelegramIntent::WorkStatus;
    }

    // Fleet / computers.
    const FLEET_PHRASES: &[&str] = &[
        "which computers",
        "what computers",
        "how many computers",
        "computer names",
        "computers are",
        "the fleet",
        "fleet status",
        "fleet nodes",
        "the nodes",
        "which nodes",
        "the machines",
        "which machines",
    ];
    if FLEET_PHRASES.iter().any(|p| t.contains(p)) {
        return TelegramIntent::FleetRoster;
    }

    // Broad fallback: any mention of computers/nodes/machines that wasn't
    // already claimed by the work check is a fleet-roster question (e.g. "we
    // have 15 computers, why are only 8 being used?"). Safe because the work
    // check ran first, so "build using all the computers" is already WorkStatus.
    if t.contains("computer") || t.contains(" node") || t.contains("machine") {
        return TelegramIntent::FleetRoster;
    }

    TelegramIntent::General
}

/// Build a TRUTHFUL fleet roster from `fleet_workers`. Never invents a name.
pub async fn answer_fleet_roster(pool: &PgPool) -> Result<String> {
    let rows = sqlx::query(
        "SELECT name, COALESCE(ip,'?') AS ip, COALESCE(status,'?') AS status, \
                COALESCE(role,'worker') AS role \
         FROM fleet_workers ORDER BY ip",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok("No computers are registered in fleet_workers.".to_string());
    }

    let online = rows
        .iter()
        .filter(|r| r.get::<String, _>("status") == "online")
        .count();
    let mut out = format!("🖥️ Fleet: {} computers ({} online)\n", rows.len(), online);
    for r in &rows {
        let name: String = r.get("name");
        let ip: String = r.get("ip");
        let status: String = r.get("status");
        let role: String = r.get("role");
        out.push_str(&format!("• {name} ({ip}) — {status}, {role}\n"));
    }
    Ok(out.trim_end().to_string())
}

/// Build a TRUTHFUL work-status answer from `work_items`: status rollup plus
/// whatever is actively building (ready/claimed/in_progress/in_review) with its
/// host. Never invents an item, a percentage, or an ETA.
pub async fn answer_work_status(pool: &PgPool) -> Result<String> {
    let counts = sqlx::query(
        "SELECT status, count(*)::bigint AS n FROM work_items GROUP BY status ORDER BY n DESC",
    )
    .fetch_all(pool)
    .await?;

    let mut rollup = String::new();
    for r in &counts {
        let status: String = r.get("status");
        let n: i64 = r.get("n");
        rollup.push_str(&format!("{status} {n} · "));
    }
    let rollup = rollup.trim_end_matches(" · ").to_string();

    let active = sqlx::query(
        "SELECT status, COALESCE(assigned_computer,'-') AS host, title \
         FROM work_items \
         WHERE status IN ('ready','claimed','in_progress','in_review') \
         ORDER BY created_at DESC LIMIT 10",
    )
    .fetch_all(pool)
    .await?;

    let mut out = format!("📋 Work items — {rollup}\n");
    if active.is_empty() {
        out.push_str("Nothing is actively building on the fleet right now.");
    } else {
        out.push_str(&format!("Active ({}):\n", active.len()));
        for r in &active {
            let status: String = r.get("status");
            let host: String = r.get("host");
            let title: String = r.get("title");
            let title = if title.chars().count() > 60 {
                let mut s: String = title.chars().take(57).collect();
                s.push_str("...");
                s
            } else {
                title
            };
            out.push_str(&format!("• [{status}] {host}: {title}\n"));
        }
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_operators_real_questions() {
        // Exactly the phrasings Vinny sent that OpenClaw hallucinated answers to.
        assert_eq!(
            classify_intent("Show me all the items you are working on and what's planned next"),
            TelegramIntent::WorkStatus
        );
        assert_eq!(
            classify_intent(
                "Show me the full list of items that ur working on now and items u have planned"
            ),
            TelegramIntent::WorkStatus
        );
        assert_eq!(
            classify_intent("for each of the items show me the eta"),
            TelegramIntent::WorkStatus
        );
        assert_eq!(
            classify_intent("We have 15 computers right why are only 8 computers being used?"),
            TelegramIntent::FleetRoster
        );
        assert_eq!(
            classify_intent("tell me which computers are being used their names"),
            TelegramIntent::FleetRoster
        );
    }

    #[test]
    fn work_status_wins_when_both_appear() {
        // "items … using all the computers" is primarily a work question.
        assert_eq!(
            classify_intent("show me the items you are working on using all the computers"),
            TelegramIntent::WorkStatus
        );
    }

    #[test]
    fn general_chat_defers_to_llm() {
        assert_eq!(classify_intent("hi how are you?"), TelegramIntent::General);
        assert_eq!(
            classify_intent("write me a haiku about rust"),
            TelegramIntent::General
        );
        assert_eq!(classify_intent(""), TelegramIntent::General);
    }
}
