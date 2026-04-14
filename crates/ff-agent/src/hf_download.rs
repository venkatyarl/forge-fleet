//! Hugging Face model repository downloader.
//!
//! Streams files from `huggingface.co` via the public resolve API, with
//! allow/deny glob filters, resumable skip-if-complete behaviour, and
//! progress callbacks.

use std::path::PathBuf;

use futures::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

/// Options for [`download_repo`].
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// e.g. `"Qwen/Qwen3-Coder-30B-A3B-Instruct-GGUF"`.
    pub repo: String,
    /// Git revision. `None` means `"main"`.
    pub revision: Option<String>,
    /// Local destination directory (created if missing).
    pub dest_dir: PathBuf,
    /// Optional HF token for gated / private models.
    pub token: Option<String>,
    /// Glob-ish include filters applied to file paths. Empty means "all".
    pub allow_patterns: Vec<String>,
    /// Glob-ish exclude filters applied after allow filters.
    pub deny_patterns: Vec<String>,
}

/// Progress tick emitted while streaming a file.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub file: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub percent: f32,
}

#[derive(Debug, Deserialize)]
struct TreeEntry {
    #[serde(rename = "type")]
    entry_type: String,
    path: String,
    #[serde(default)]
    size: u64,
}

/// Download a repository from Hugging Face.
///
/// Returns the list of files successfully materialised on disk.
pub async fn download_repo(
    opts: DownloadOptions,
    mut progress: impl FnMut(DownloadProgress) + Send + 'static,
) -> Result<Vec<PathBuf>, String> {
    let revision = opts.revision.clone().unwrap_or_else(|| "main".to_string());

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        // No overall timeout: model files are multi-GB.
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))?;

    // 1. List tree (recursive).
    let list_url = format!(
        "https://huggingface.co/api/models/{}/tree/{}?recursive=true",
        opts.repo, revision
    );
    let mut req = client.get(&list_url);
    if let Some(tok) = &opts.token {
        req = req.bearer_auth(tok);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("failed to list repo {}: {e}", opts.repo))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(format!(
            "401 Unauthorized listing {} — HF token required",
            opts.repo
        ));
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "403 Forbidden listing {} — gated repo, token lacks access",
            opts.repo
        ));
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "HF tree API returned {status} for {}: {body}",
            opts.repo
        ));
    }

    let entries: Vec<TreeEntry> = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse HF tree JSON: {e}"))?;

    // 2. Filter.
    let files: Vec<TreeEntry> = entries
        .into_iter()
        .filter(|e| e.entry_type == "file")
        .filter(|e| passes_filters(&e.path, &opts.allow_patterns, &opts.deny_patterns))
        .collect();

    if files.is_empty() {
        return Err(format!(
            "no files matched in {} (after allow/deny filters)",
            opts.repo
        ));
    }

    tokio::fs::create_dir_all(&opts.dest_dir)
        .await
        .map_err(|e| format!("failed to create dest dir {:?}: {e}", opts.dest_dir))?;

    let mut downloaded = Vec::with_capacity(files.len());

    // 3. Download each file.
    for file in files {
        let dest_path = opts.dest_dir.join(&file.path);
        if let Some(parent) = dest_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("failed to create parent {:?}: {e}", parent))?;
        }

        // Resume: skip if already complete.
        if file.size > 0 {
            if let Ok(meta) = tokio::fs::metadata(&dest_path).await {
                if meta.is_file() && meta.len() == file.size {
                    progress(DownloadProgress {
                        file: file.path.clone(),
                        bytes_done: file.size,
                        bytes_total: file.size,
                        percent: 100.0,
                    });
                    downloaded.push(dest_path);
                    continue;
                }
            }
        }

        let url = format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            opts.repo, revision, file.path
        );
        let mut req = client.get(&url);
        if let Some(tok) = &opts.token {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("GET {url} failed: {e}"))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(format!("401 Unauthorized downloading {} — token required", file.path));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(format!(
                "403 Forbidden downloading {} — gated, token lacks access",
                file.path
            ));
        }
        if !status.is_success() {
            return Err(format!("GET {url} returned {status}"));
        }

        let total = resp.content_length().unwrap_or(file.size);

        let mut out = tokio::fs::File::create(&dest_path)
            .await
            .map_err(|e| format!("failed to open {:?} for write: {e}", dest_path))?;

        let mut stream = resp.bytes_stream();
        let mut bytes_done: u64 = 0;
        let mut last_pct_tick: i32 = -1;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("stream error on {}: {e}", file.path))?;
            out.write_all(&chunk).await.map_err(|e| {
                let msg = e.to_string();
                if e.raw_os_error() == Some(28) /* ENOSPC */ {
                    format!("disk full writing {:?}: {msg}", dest_path)
                } else {
                    format!("write error on {:?}: {msg}", dest_path)
                }
            })?;
            bytes_done += chunk.len() as u64;

            let pct = if total > 0 {
                (bytes_done as f64 / total as f64 * 100.0) as f32
            } else {
                0.0
            };
            let tick = pct as i32;
            if tick != last_pct_tick {
                last_pct_tick = tick;
                progress(DownloadProgress {
                    file: file.path.clone(),
                    bytes_done,
                    bytes_total: total,
                    percent: pct,
                });
            }
        }

        out.flush()
            .await
            .map_err(|e| format!("flush failed on {:?}: {e}", dest_path))?;
        drop(out);

        // Final tick.
        progress(DownloadProgress {
            file: file.path.clone(),
            bytes_done,
            bytes_total: total,
            percent: 100.0,
        });

        // 4. Verify size if known.
        if file.size > 0 && bytes_done != file.size {
            return Err(format!(
                "size mismatch for {}: expected {}, got {}",
                file.path, file.size, bytes_done
            ));
        }

        downloaded.push(dest_path);
    }

    Ok(downloaded)
}

/// Returns true if `path` should be kept given allow/deny glob-ish patterns.
fn passes_filters(path: &str, allow: &[String], deny: &[String]) -> bool {
    if !allow.is_empty() && !allow.iter().any(|p| glob_match(p, path)) {
        return false;
    }
    if deny.iter().any(|p| glob_match(p, path)) {
        return false;
    }
    true
}

/// Minimal glob matcher: supports `*` as a multi-char wildcard (including `/`).
/// Handles common HF filter shapes: `*.gguf`, `tokenizer*`, `*.safetensors`,
/// `*config*`, literal paths, etc.
fn glob_match(pattern: &str, text: &str) -> bool {
    // Match against the full path; also try basename for bare patterns.
    if glob_match_inner(pattern, text) {
        return true;
    }
    if !pattern.contains('/') {
        if let Some(base) = text.rsplit('/').next() {
            if glob_match_inner(pattern, base) {
                return true;
            }
        }
    }
    false
}

fn glob_match_inner(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    fn rec(p: &[char], t: &[char]) -> bool {
        let mut pi = 0usize;
        let mut ti = 0usize;
        let mut star: Option<(usize, usize)> = None;
        while ti < t.len() {
            if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
                pi += 1;
                ti += 1;
            } else if pi < p.len() && p[pi] == '*' {
                star = Some((pi, ti));
                pi += 1;
            } else if let Some((sp, st)) = star {
                pi = sp + 1;
                ti = st + 1;
                star = Some((sp, ti));
            } else {
                return false;
            }
        }
        while pi < p.len() && p[pi] == '*' {
            pi += 1;
        }
        pi == p.len()
    }
    rec(&pat, &txt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match("*.gguf", "model-Q4_K_M.gguf"));
        assert!(glob_match("*.gguf", "subdir/model.gguf"));
        assert!(!glob_match("*.gguf", "model.safetensors"));
        assert!(glob_match("tokenizer*", "tokenizer.json"));
        assert!(glob_match("*config*", "generation_config.json"));
        assert!(glob_match("*.safetensors", "model-00001-of-00010.safetensors"));
    }

    #[test]
    fn filter_allow_deny() {
        let allow = vec!["*.gguf".to_string()];
        let deny = vec!["*f16*".to_string()];
        assert!(passes_filters("qwen-Q4_K_M.gguf", &allow, &deny));
        assert!(!passes_filters("qwen-f16.gguf", &allow, &deny));
        assert!(!passes_filters("README.md", &allow, &deny));
    }
}
