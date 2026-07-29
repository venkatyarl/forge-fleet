use std::path::Path;

use anyhow::{Context, Result};
use serde_json::json;
use sqlx::PgPool;

fn operator_identity() -> Result<String> {
    validate_operator_identity(std::env::var("FORGEFLEET_OPERATOR_IDENTITY").ok())
        .context("attach requires FORGEFLEET_OPERATOR_IDENTITY")
}

fn validate_operator_identity(identity: Option<String>) -> Result<String> {
    let identity = identity.context("operator fleet identity is unavailable")?;
    if identity.trim().is_empty() {
        anyhow::bail!("FORGEFLEET_OPERATOR_IDENTITY must not be empty");
    }
    Ok(identity)
}

async fn resolve(
    pool: &PgPool,
    cwd: &Path,
    project: Option<String>,
) -> Result<ff_agent::workstreams::Workstream> {
    match project {
        Some(project) => ff_agent::workstreams::workstream_for_project(pool, &project).await?,
        None => ff_agent::workstreams::workstream_for_dir(pool, cwd).await?,
    }
    .ok_or_else(|| anyhow::anyhow!("no authorized workstream resolves for this project"))
}

fn workstream_json(ws: &ff_agent::workstreams::Workstream) -> serde_json::Value {
    json!({
        "project_id": ws.project_id,
        "git_remote": ws.git_remote,
        "basename": ws.basename,
        "goal": ws.goal,
        "working_summary": ws.working_summary,
        "focus": ws.focus,
        "open_threads": ws.open_threads,
        "status": ws.status,
        "leader_generation": ws.leader_generation,
    })
}

pub async fn status(pool: &PgPool, cwd: &Path, project: Option<String>) -> Result<()> {
    let operator = operator_identity()?;
    let ws = resolve(pool, cwd, project).await?;
    ff_agent::workstreams::authorize_operator(&ws, &operator)?;
    let presence = ff_agent::workstreams::live_presence(pool, &ws, &operator).await?;
    let mut packet = workstream_json(&ws);
    packet["live_presence"] = serde_json::to_value(presence)?;
    println!("{}", serde_json::to_string_pretty(&packet)?);
    Ok(())
}

pub async fn attach(pool: &PgPool, cwd: &Path, project: Option<String>) -> Result<()> {
    let operator = operator_identity()?;
    let ws = resolve(pool, cwd, project).await?;
    ff_agent::workstreams::authorize_operator(&ws, &operator)?;
    let resume = ff_agent::workstreams::attach_resume_context(pool, &ws, &operator).await?;
    let mut packet = workstream_json(&ws);
    packet["operator_identity"] = json!(operator);
    packet["causal_watermark"] = json!(resume.causal_watermark);
    packet["notes"] = serde_json::to_value(resume.notes)?;
    packet["live_presence"] = serde_json::to_value(resume.live_presence)?;
    println!("{}", serde_json::to_string_pretty(&packet)?);
    Ok(())
}

pub async fn note(pool: &PgPool, cwd: &Path, project: Option<String>, text: String) -> Result<()> {
    let operator = operator_identity()?;
    let ws = resolve(pool, cwd, project).await?;
    ff_agent::workstreams::authorize_operator(&ws, &operator)?;
    let seq = ff_agent::workstreams::append_note(pool, &ws, &operator, &text).await?;
    println!("\u{1b}[32m✓\u{1b}[0m note appended (seq {seq})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_operator_identity;

    #[test]
    fn operator_identity_is_mandatory_for_durable_session_access() {
        assert!(validate_operator_identity(None).is_err());
        assert!(validate_operator_identity(Some(" ".into())).is_err());
        assert_eq!(
            validate_operator_identity(Some("operator:fleet".into())).unwrap(),
            "operator:fleet"
        );
    }
}
