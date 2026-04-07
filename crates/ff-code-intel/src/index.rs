//! Codebase indexer — walk a directory tree, parse files, build the graph.

use std::path::Path;

use tokio::fs;
use tracing::{debug, info};

use crate::graph::CodeGraph;
use crate::parser::{self, Language};

/// Index an entire directory, building a code knowledge graph.
pub async fn index_directory(dir: &Path) -> anyhow::Result<CodeGraph> {
    let mut graph = CodeGraph::new();
    let mut file_count = 0;
    let mut entity_count = 0;

    walk_and_index(dir, dir, &mut graph, &mut file_count, &mut entity_count).await?;

    info!(files = file_count, entities = entity_count, "codebase indexed");
    Ok(graph)
}

async fn walk_and_index(
    base: &Path,
    dir: &Path,
    graph: &mut CodeGraph,
    file_count: &mut usize,
    entity_count: &mut usize,
) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        // Skip hidden dirs and common noise
        if name.starts_with('.') || SKIP_DIRS.contains(&name) {
            continue;
        }

        if path.is_dir() {
            Box::pin(walk_and_index(base, &path, graph, file_count, entity_count)).await?;
        } else {
            let lang = Language::from_path(&path);
            if lang == Language::Unknown {
                continue;
            }

            let content = match fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(_) => continue, // skip binary/unreadable files
            };

            let file_path = path.to_string_lossy().to_string();
            let hash = simple_hash(&content);

            if graph.needs_reindex(&file_path, &hash) {
                let entities = parser::extract_entities(&content, &file_path, lang);
                let count = entities.len();
                graph.index_file(&file_path, entities, &hash);
                *file_count += 1;
                *entity_count += count;
                debug!(file = %file_path, entities = count, "indexed");
            }
        }
    }

    Ok(())
}

const SKIP_DIRS: &[&str] = &[
    "node_modules", "target", ".git", "__pycache__", ".next",
    "dist", "build", "vendor", ".venv", "venv", ".tox",
    "coverage", ".nyc_output", ".cache",
];

fn simple_hash(content: &str) -> String {
    // Simple FNV-1a hash for change detection (not cryptographic)
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in content.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn index_forge_fleet_crate() {
        // Index just the ff-agent/src/tools directory as a quick test
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .join("ff-agent").join("src").join("tools");

        if !dir.exists() {
            return; // skip if not in workspace
        }

        let graph = index_directory(&dir).await.unwrap();
        let stats = graph.stats();
        assert!(stats.total_entities > 0, "should find entities in tools/");
        assert!(stats.total_files > 0, "should index files");
    }
}
