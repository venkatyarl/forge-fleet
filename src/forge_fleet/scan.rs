//! Native implementation of `ff project scan`.

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

const SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    ".next",
    ".venv",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "target",
    "vendor",
];

/// Metadata collected from a local project checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectScan {
    pub source: PathBuf,
    pub github_url: String,
    pub tech_stack: String,
}

/// Scan a project checkout without invoking a shell traversal.
///
/// The source path is canonicalized, generated/dependency directories are
/// pruned, and Git is invoked directly only to read the checkout's origin.
pub fn scan_project(path: impl AsRef<Path>) -> Result<ProjectScan> {
    let requested = path.as_ref();
    let source = std::fs::canonicalize(requested)
        .with_context(|| format!("scan {}", requested.display()))?;
    if !source.is_dir() {
        bail!("not a directory: {}", source.display());
    }

    let tech_stack = detect_tech_stack(&source)?;
    let github_url = git_origin(&source)
        .ok_or_else(|| anyhow::anyhow!("no git origin found from {}", source.display()))?;

    Ok(ProjectScan {
        source,
        github_url,
        tech_stack,
    })
}

/// Return the dominant recognized source language below `root`.
pub fn detect_tech_stack(root: &Path) -> Result<String> {
    let mut counts = BTreeMap::<&'static str, usize>::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
            let file_type = entry
                .file_type()
                .with_context(|| format!("read file type for {}", entry.path().display()))?;
            let path = entry.path();

            if file_type.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if !SKIPPED_DIRECTORIES.contains(&name) {
                    stack.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let language = match path.extension().and_then(|extension| extension.to_str()) {
                Some("rs") => Some("rust"),
                Some("py") => Some("python"),
                Some("ts" | "tsx") => Some("typescript"),
                Some("js" | "jsx") => Some("javascript"),
                Some("go") => Some("go"),
                Some("java") => Some("java"),
                Some("rb") => Some("ruby"),
                Some("php") => Some("php"),
                Some("swift") => Some("swift"),
                Some("kt" | "kts") => Some("kotlin"),
                _ => None,
            };
            if let Some(language) = language {
                *counts.entry(language).or_default() += 1;
            }
        }
    }

    counts
        .into_iter()
        .max_by(
            |(left_language, left_count), (right_language, right_count)| {
                left_count
                    .cmp(right_count)
                    .then_with(|| right_language.cmp(left_language))
            },
        )
        .map(|(language, _)| language.to_owned())
        .ok_or_else(|| anyhow::anyhow!("no recognized source files under {}", root.display()))
}

fn git_origin(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!url.is_empty()).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_dominant_stack_and_prunes_generated_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("one.py"), "").unwrap();
        std::fs::write(root.path().join("two.py"), "").unwrap();
        std::fs::write(root.path().join("other.rs"), "").unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        for index in 0..4 {
            std::fs::write(root.path().join("target").join(format!("{index}.rs")), "").unwrap();
        }

        assert_eq!(detect_tech_stack(root.path()).unwrap(), "python");
    }

    #[test]
    fn reports_an_empty_source_tree() {
        let root = tempfile::tempdir().unwrap();
        let error = detect_tech_stack(root.path()).unwrap_err().to_string();

        assert!(error.contains("no recognized source files"));
    }
}
