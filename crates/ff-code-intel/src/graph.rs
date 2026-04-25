//! Code knowledge graph — relationships between code entities.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::parser::{CodeEntity, EntityKind};

/// Edge type in the code graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Calls,
    CalledBy,
    Imports,
    ImportedBy,
    Contains,
    ContainedBy,
    Implements,
    ImplementedBy,
    DependsOn,
    DependedOnBy,
}

/// An edge in the code graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub file_path: String,
    pub line: usize,
}

/// The code knowledge graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeGraph {
    /// All entities keyed by fully-qualified name.
    pub entities: HashMap<String, CodeEntity>,
    /// All edges.
    pub edges: Vec<Edge>,
    /// File hashes for incremental indexing.
    pub file_hashes: HashMap<String, String>,
}

impl CodeGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add entities from a file. Replaces previous entities for that file.
    pub fn index_file(&mut self, file_path: &str, entities: Vec<CodeEntity>, file_hash: &str) {
        // Remove old entities from this file
        self.entities.retain(|_, e| e.file_path != file_path);
        self.edges.retain(|e| e.file_path != file_path);

        // Add new entities
        for entity in entities {
            let key = format!("{}:{}", file_path, entity.name);
            self.entities.insert(key, entity);
        }

        self.file_hashes
            .insert(file_path.to_string(), file_hash.to_string());
    }

    /// Check if a file needs re-indexing (hash changed).
    pub fn needs_reindex(&self, file_path: &str, current_hash: &str) -> bool {
        self.file_hashes
            .get(file_path)
            .map(|h| h != current_hash)
            .unwrap_or(true)
    }

    /// Find all entities matching a query (case-insensitive name search).
    pub fn search(&self, query: &str) -> Vec<&CodeEntity> {
        let lower = query.to_ascii_lowercase();
        self.entities
            .values()
            .filter(|e| e.name.to_ascii_lowercase().contains(&lower))
            .collect()
    }

    /// Find all entities of a specific kind.
    pub fn find_by_kind(&self, kind: EntityKind) -> Vec<&CodeEntity> {
        self.entities.values().filter(|e| e.kind == kind).collect()
    }

    /// Find all entities in a specific file.
    pub fn find_in_file(&self, file_path: &str) -> Vec<&CodeEntity> {
        self.entities
            .values()
            .filter(|e| e.file_path == file_path)
            .collect()
    }

    /// Get callers of a function (entities that call it).
    pub fn callers_of(&self, name: &str) -> Vec<&Edge> {
        self.edges
            .iter()
            .filter(|e| e.to == name && e.kind == EdgeKind::CalledBy)
            .collect()
    }

    /// Get callees of a function (entities it calls).
    pub fn callees_of(&self, name: &str) -> Vec<&Edge> {
        self.edges
            .iter()
            .filter(|e| e.from == name && e.kind == EdgeKind::Calls)
            .collect()
    }

    /// Summary statistics.
    pub fn stats(&self) -> GraphStats {
        let mut by_kind: HashMap<EntityKind, usize> = HashMap::new();
        for entity in self.entities.values() {
            *by_kind.entry(entity.kind).or_insert(0) += 1;
        }
        GraphStats {
            total_entities: self.entities.len(),
            total_edges: self.edges.len(),
            total_files: self.file_hashes.len(),
            by_kind,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphStats {
    pub total_entities: usize,
    pub total_edges: usize,
    pub total_files: usize,
    pub by_kind: HashMap<EntityKind, usize>,
}
