//! Orchestrates the work-item indexing pipeline: read Markdown docs under a
//! plan path, extract candidate strings, derive typed work items, and map
//! them onto the shared schema type used across the fleet.

use std::fs;
use std::path::{Path, PathBuf};

use ff_core::schema::work_item::{WorkItem, WorkItemStatus};

use super::md_extractor::extract_candidates;
use super::work_item_deriver::derive_work_items;

/// Rebuild the work-item index from every Markdown doc under `plan_path`.
///
/// Walks `plan_path` (a single `.md` file or a directory tree) for Markdown
/// docs, extracts candidate strings from each with [`super::md_extractor`],
/// derives typed work items with [`super::work_item_deriver`], and
/// consolidates them into the shared [`ff_core::schema::work_item::WorkItem`]
/// type. Unreadable docs are skipped rather than failing the whole refresh.
pub fn refresh_work_item_index(plan_path: &Path) -> Vec<WorkItem> {
    collect_md_files(plan_path)
        .into_iter()
        .flat_map(|doc_path| {
            let Ok(content) = fs::read_to_string(&doc_path) else {
                return Vec::new();
            };
            let candidates = extract_candidates(&content);
            let source = doc_path.display().to_string();
            derive_work_items(&candidates, &source)
                .into_iter()
                .map(to_schema_work_item)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Recursively collect `.md` files under `root`. If `root` is itself a `.md`
/// file, return just that file.
fn collect_md_files(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return if is_md(root) {
            vec![root.to_path_buf()]
        } else {
            Vec::new()
        };
    }

    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && is_md(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn is_md(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Map the doc-local candidate-derivation type onto the shared schema type.
///
/// Freshly derived candidates haven't been triaged, so they map to
/// [`WorkItemStatus::Backlog`] rather than `Todo`.
fn to_schema_work_item(item: super::work_item_deriver::WorkItem) -> WorkItem {
    WorkItem {
        id: item.id,
        title: item.title,
        description: String::new(),
        status: WorkItemStatus::Backlog,
        source_ref: item.source,
        derived_at: item.created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ff-brain-index-test-{name}-{:p}", &name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn refreshes_work_items_from_a_single_md_file() {
        let dir = temp_dir("single-file");
        let doc_path = dir.join("plan.md");
        fs::write(&doc_path, "- [ ] Wire up the indexer\nAction: ship it\n").unwrap();

        let items = refresh_work_item_index(&doc_path);

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Wire up the indexer");
        assert_eq!(items[1].title, "ship it");
        assert!(
            items
                .iter()
                .all(|item| item.status == WorkItemStatus::Backlog)
        );
        assert!(
            items
                .iter()
                .all(|item| item.source_ref == doc_path.display().to_string())
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn walks_nested_directories_and_ignores_non_md_files() {
        let dir = temp_dir("nested-dir");
        let sub_dir = dir.join("sub");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(dir.join("root.md"), "- [ ] Root task\n").unwrap();
        fs::write(sub_dir.join("nested.md"), "- [ ] Nested task\n").unwrap();
        fs::write(sub_dir.join("ignore.txt"), "- [ ] Should not appear\n").unwrap();

        let mut titles: Vec<_> = refresh_work_item_index(&dir)
            .into_iter()
            .map(|item| item.title)
            .collect();
        titles.sort();

        assert_eq!(
            titles,
            vec!["Nested task".to_string(), "Root task".to_string()]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_path_returns_empty_vec() {
        let missing = std::env::temp_dir().join("ff-brain-index-test-does-not-exist");
        assert!(refresh_work_item_index(&missing).is_empty());
    }
}
