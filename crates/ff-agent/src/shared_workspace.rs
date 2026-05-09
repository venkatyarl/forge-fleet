//! Shared Workspace + Sub-Agent Model (Phase 15e)
//!
//! Manages hierarchical workspace directories for agents and sub-agents.
//! Each sub-agent gets an isolated workspace under:
//! ~/.forgefleet/agents/agent-{id}/sub-agents/sub-agent-{m}/

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{info, warn};

/// A shared workspace reference stored in the fleet_workspaces table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedWorkspace {
    pub workspace_id: String,
    pub owner_node: String,
    pub workspace_path: PathBuf,
    pub sync_method: String,
}

/// Sub-agent workspace paths.
#[derive(Debug, Clone)]
pub struct SubAgentWorkspace {
    pub agent_id: String,
    pub subagent_id: String,
    pub base_path: PathBuf,
}

impl SubAgentWorkspace {
    /// Create workspace paths for a sub-agent.
    pub fn new(agent_id: &str, subagent_id: &str) -> Self {
        let base = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".forgefleet")
            .join("agents")
            .join(agent_id)
            .join("sub-agents")
            .join(subagent_id);

        Self {
            agent_id: agent_id.to_string(),
            subagent_id: subagent_id.to_string(),
            base_path: base,
        }
    }

    pub fn work_dir(&self) -> PathBuf {
        self.base_path.join("work")
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.base_path.join("repos")
    }

    pub fn artifacts_pending_dir(&self) -> PathBuf {
        self.base_path.join("artifacts").join("pending")
    }

    pub fn artifacts_promoted_dir(&self) -> PathBuf {
        self.base_path.join("artifacts").join("promoted")
    }

    pub fn temp_dir(&self) -> PathBuf {
        self.base_path.join("temp")
    }

    pub fn metadata_path(&self) -> PathBuf {
        self.base_path.join(".metadata.json")
    }

    /// Create all workspace directories.
    pub async fn create(&self) -> Result<()> {
        fs::create_dir_all(self.work_dir()).await?;
        fs::create_dir_all(self.repos_dir()).await?;
        fs::create_dir_all(self.artifacts_pending_dir()).await?;
        fs::create_dir_all(self.artifacts_promoted_dir()).await?;
        fs::create_dir_all(self.temp_dir()).await?;

        let metadata = serde_json::json!({
            "agent_id": self.agent_id,
            "subagent_id": self.subagent_id,
            "created_at": chrono::Utc::now().to_rfc3339(),
            "status": "active",
        });
        fs::write(
            self.metadata_path(),
            serde_json::to_string_pretty(&metadata)?,
        )
        .await?;

        info!(path = %self.base_path.display(), "sub-agent workspace created");
        Ok(())
    }

    /// Clear temp/ directory.
    pub async fn clear_temp(&self) -> Result<()> {
        if self.temp_dir().exists() {
            fs::remove_dir_all(self.temp_dir()).await?;
            fs::create_dir_all(self.temp_dir()).await?;
        }
        Ok(())
    }

    /// Promote an artifact from pending/ to promoted/.
    pub async fn promote_artifact(&self, name: &str) -> Result<PathBuf> {
        let src = self.artifacts_pending_dir().join(name);
        let dst = self.artifacts_promoted_dir().join(name);

        if !src.exists() {
            anyhow::bail!("Artifact not found in pending: {}", name);
        }

        fs::copy(&src, &dst).await?;
        info!(artifact = name, "artifact promoted");
        Ok(dst)
    }
}

/// Create the full agent + sub-agent directory hierarchy.
pub async fn setup_agent_workspace(agent_id: &str) -> Result<PathBuf> {
    let base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".forgefleet")
        .join("agents")
        .join(agent_id);

    for dir in &[
        "config",
        "memory",
        "sessions",
        "checkpoints",
        "workspace",
        "sub-agents",
        "logs",
    ] {
        fs::create_dir_all(base.join(dir)).await?;
    }

    // Write agent metadata
    let metadata = serde_json::json!({
        "agent_id": agent_id,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "role": "general",
    });
    fs::write(
        base.join(".metadata.json"),
        serde_json::to_string_pretty(&metadata)?,
    )
    .await?;

    info!(agent_id = agent_id, path = %base.display(), "agent workspace created");
    Ok(base)
}

/// Daily cleanup: remove temp, old pending artifacts, stale git clones.
/// Also logs cleanup actions to `subagent_cleanup_log`.
pub async fn run_cleanup(
    agent_id: &str,
    pg: Option<&sqlx::PgPool>,
    node_id: Option<uuid::Uuid>,
) -> Result<CleanupReport> {
    let agent_base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".forgefleet")
        .join("agents")
        .join(agent_id);

    let mut report = CleanupReport::default();

    // 1. Clear all temp/ dirs
    let subagents_dir = agent_base.join("sub-agents");
    if subagents_dir.exists() {
        let mut entries = fs::read_dir(&subagents_dir).await?;
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let path = entry.path();
                    let subagent_id = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let temp = path.join("temp");
                    if temp.exists() {
                        match fs::remove_dir_all(&temp).await {
                            Ok(_) => {
                                report.temp_cleared += 1;
                                fs::create_dir_all(&temp).await?;
                                if let (Some(pool), Some(nid)) = (pg, node_id) {
                                    let _ = sqlx::query(
                                        r#"
                                        INSERT INTO subagent_cleanup_log
                                            (node_id, subagent_id, item_type, item_path, reason)
                                        VALUES ($1, $2, 'temp', $3, 'daily_cleanup')
                                        "#,
                                    )
                                    .bind(nid)
                                    .bind(&subagent_id)
                                    .bind(temp.to_string_lossy().as_ref())
                                    .execute(pool)
                                    .await;
                                }
                            }
                            Err(e) => {
                                warn!(path = %temp.display(), error = %e, "failed to clear temp")
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }

    // 2. Remove pending artifacts older than 30 days (placeholder — would check mtime)
    // 3. Remove git clones older than 60 days (placeholder)

    info!(agent_id = agent_id, report = ?report, "cleanup complete");
    Ok(report)
}

#[derive(Debug, Default)]
pub struct CleanupReport {
    pub temp_cleared: u32,
    pub artifacts_removed: u32,
    pub git_folders_removed: u32,
    pub bytes_freed: u64,
}

/// Upsert a workspace record into `fleet_workspaces`.
pub async fn upsert_fleet_workspace(
    pg: &sqlx::PgPool,
    node_id: uuid::Uuid,
    workspace_path: &Path,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO fleet_workspaces (owner_node_id, workspace_path, sync_method, last_synced_at)
        VALUES ($1, $2, 'git', NOW())
        "#,
    )
    .bind(node_id)
    .bind(workspace_path.to_string_lossy().as_ref())
    .execute(pg)
    .await?;
    Ok(())
}

/// List all agent IDs that have a workspace directory.
pub async fn list_agent_workspaces() -> Result<Vec<String>> {
    let base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".forgefleet")
        .join("agents");

    let mut agents = Vec::new();
    if !base.exists() {
        return Ok(agents);
    }

    let mut entries = fs::read_dir(&base).await?;
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                let path = entry.path();
                if path.is_dir()
                    && let Some(name) = path.file_name().and_then(|n| n.to_str())
                {
                    agents.push(name.to_string());
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    Ok(agents)
}
