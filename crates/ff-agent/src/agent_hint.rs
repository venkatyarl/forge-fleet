//! Agent hint loader (V67) — pulls `software_registry.agent_hint` strings
//! for software installed on this host and concatenates them into a
//! single block the supervise/run dispatch prepends to the agent's
//! system prompt.
//!
//! Operator directive 2026-04-30: "I dont want to add more commands... I
//! want ff to decide when to use it." The agent reads the hint and
//! routes itself — no `ff design` verb, no separate intent classifier,
//! no special-cased software in the dispatcher.

use anyhow::Result;
use sqlx::PgPool;

/// Load agent hints for software currently installed (`status='ok'`) on
/// the named computer. Returns an empty string when no relevant rows
/// exist — caller can prepend unconditionally.
pub async fn load_for_host(pool: &PgPool, computer_name: &str) -> Result<String> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        r#"
        SELECT sr.id, sr.display_name, sr.agent_hint
          FROM software_registry sr
          JOIN computer_software cs ON cs.software_id = sr.id
          JOIN computers c ON c.id = cs.computer_id
         WHERE LOWER(c.name) = LOWER($1)
           AND cs.status = 'ok'
           AND sr.agent_hint IS NOT NULL
           AND sr.agent_hint <> ''
         ORDER BY sr.id
        "#,
    )
    .bind(computer_name)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(String::new());
    }

    let mut out = String::from(
        "## Tools available on this machine (auto-injected by ForgeFleet)\n\n\
         The following software is pre-installed and ready to use without further setup. \
         Decide for yourself whether the user's request warrants using any of them.\n\n",
    );
    for (id, display_name, hint) in rows {
        out.push_str(&format!("### {display_name} (`{id}`)\n{hint}\n\n"));
    }
    Ok(out)
}

/// Convenience: prepend hints to an existing system prompt. Returns the
/// prompt unchanged if the hints block is empty (no installed
/// hint-bearing software).
pub fn prepend_to_system_prompt(hints: &str, existing: Option<String>) -> Option<String> {
    let existing = existing.unwrap_or_default();
    if hints.is_empty() {
        if existing.is_empty() {
            None
        } else {
            Some(existing)
        }
    } else if existing.is_empty() {
        Some(hints.to_string())
    } else {
        Some(format!("{hints}\n---\n\n{existing}"))
    }
}
