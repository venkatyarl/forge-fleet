use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::store::{Memory, MemoryStore, NewMemory, SearchMemoriesParams};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceProfile {
    pub id: String,
    pub company: Option<String>,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl WorkspaceProfile {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            company: None,
            description: None,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSearchHit {
    pub workspace_id: String,
    pub memory: Memory,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct WorkspaceScopedStore {
    workspace_id: String,
    store: MemoryStore,
}

impl WorkspaceScopedStore {
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub async fn save(&self, mut memory: NewMemory) -> Result<Memory> {
        memory.workspace_id = self.workspace_id.clone();
        Ok(self.store.save_memory(memory).await?)
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<Memory>> {
        let memory = self.store.get_memory(id).await?;
        Ok(memory.filter(|m| m.workspace_id == self.workspace_id))
    }

    pub async fn search(
        &self,
        keyword: Option<String>,
        tags: Vec<String>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        Ok(self
            .store
            .search_memories(SearchMemoriesParams {
                workspace_id: Some(self.workspace_id.clone()),
                keyword,
                tags,
                source: None,
                min_importance: None,
                since: None,
                limit,
            })
            .await?)
    }

    pub async fn delete(&self, id: Uuid) -> Result<bool> {
        if let Some(memory) = self.store.get_memory(id).await? {
            if memory.workspace_id != self.workspace_id {
                return Ok(false);
            }
            return Ok(self.store.delete_memory(id).await?);
        }
        Ok(false)
    }
}

#[derive(Debug)]
pub struct WorkspaceMemoryManager {
    store: MemoryStore,
    workspaces: DashMap<String, WorkspaceProfile>,
}

impl WorkspaceMemoryManager {
    pub fn new(store: MemoryStore) -> Self {
        Self {
            store,
            workspaces: DashMap::new(),
        }
    }

    pub fn register_workspace(&self, profile: WorkspaceProfile) {
        self.workspaces.insert(profile.id.clone(), profile);
    }

    pub fn get_workspace(&self, workspace_id: &str) -> Option<WorkspaceProfile> {
        self.workspaces.get(workspace_id).map(|w| w.clone())
    }

    pub fn list_workspaces(&self) -> Vec<WorkspaceProfile> {
        self.workspaces
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn scoped_store(&self, workspace_id: impl Into<String>) -> WorkspaceScopedStore {
        let workspace_id = workspace_id.into();

        if !self.workspaces.contains_key(&workspace_id) {
            self.register_workspace(WorkspaceProfile::new(workspace_id.clone()));
        }

        WorkspaceScopedStore {
            workspace_id,
            store: self.store.clone(),
        }
    }

    pub async fn save_memory(&self, workspace_id: &str, mut memory: NewMemory) -> Result<Memory> {
        if !self.workspaces.contains_key(workspace_id) {
            self.register_workspace(WorkspaceProfile::new(workspace_id.to_string()));
        }

        memory.workspace_id = workspace_id.to_string();
        Ok(self.store.save_memory(memory).await?)
    }

    pub async fn search_workspace(
        &self,
        workspace_id: &str,
        keyword: Option<String>,
        tags: Vec<String>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        Ok(self
            .store
            .search_memories(SearchMemoriesParams {
                workspace_id: Some(workspace_id.to_string()),
                keyword,
                tags,
                source: None,
                min_importance: None,
                since: None,
                limit,
            })
            .await?)
    }

    pub async fn cross_workspace_search(
        &self,
        workspace_ids: Option<Vec<String>>,
        keyword: Option<String>,
        tags: Vec<String>,
        limit_per_workspace: usize,
    ) -> Result<Vec<WorkspaceSearchHit>> {
        let target_workspaces = workspace_ids.unwrap_or_else(|| {
            let known = self.list_workspaces();
            known.into_iter().map(|w| w.id).collect()
        });

        let mut hits = Vec::new();

        if target_workspaces.is_empty() {
            let memories = self
                .store
                .search_memories(SearchMemoriesParams {
                    workspace_id: None,
                    keyword,
                    tags,
                    source: None,
                    min_importance: None,
                    since: None,
                    limit: limit_per_workspace.clamp(1, 500),
                })
                .await?;

            for memory in memories {
                hits.push(WorkspaceSearchHit {
                    workspace_id: memory.workspace_id.clone(),
                    score: workspace_hit_score(&memory),
                    memory,
                });
            }
        } else {
            for workspace_id in target_workspaces {
                let memories = self
                    .store
                    .search_memories(SearchMemoriesParams {
                        workspace_id: Some(workspace_id.clone()),
                        keyword: keyword.clone(),
                        tags: tags.clone(),
                        source: None,
                        min_importance: None,
                        since: None,
                        limit: limit_per_workspace.clamp(1, 200),
                    })
                    .await?;

                for memory in memories {
                    hits.push(WorkspaceSearchHit {
                        workspace_id: workspace_id.clone(),
                        score: workspace_hit_score(&memory),
                        memory,
                    });
                }
            }
        }

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(hits)
    }
}

fn workspace_hit_score(memory: &Memory) -> f32 {
    let age_days = (Utc::now() - memory.created_at).num_days().max(0) as f32;
    let recency = (1.0 / (1.0 + age_days / 10.0)).clamp(0.0, 1.0);
    (memory.importance * 0.8) + (recency * 0.2)
}
