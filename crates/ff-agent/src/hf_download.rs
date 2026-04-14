//! Hugging Face model repository downloader.
//!
//! Streams files from `huggingface.co` via the public resolve API, with
//! allow/deny glob filters, resumable skip-if-complete behaviour, and
//! progress callbacks.

use std::path::{Path, PathBuf};

use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
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
    /// If true, skip sha256 verification of LFS files.
    pub skip_verify: bool,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            repo: String::new(),
            revision: None,
            dest_dir: PathBuf::new(),
            token: None,
            allow_patterns: Vec::new(),
            deny_patterns: Vec::new(),
            skip_verify: false,
        }
    }
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
struct LfsInfo {
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TreeEntry {
    #[serde(rename = "type")]
    entry_type: String,
    path: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    lfs: Option<LfsInfo>,
}

impl TreeEntry {
    fn expected_sha256(&self) -> Option<&str> {
        self.lfs.as_ref().and_then(|l| l.sha256.as_deref())
    }
}

/// Compute sha256 of a file via streaming reads (64 KiB chunks).
///
/// Returns the lowercase hex digest.
pub fn compute_file_sha256(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .map_err(|e| format!("failed to open {:?} for hashing: {e}", path))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("read error while hashing {:?}: {e}", path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(hex_encode(&digest))
}

/// Verify a file's sha256 against `expected` (case-insensitive hex).
///
/// Returns `Ok(true)` if they match, `Ok(false)` if they don't, `Err` on I/O.
pub fn verify_file_sha256(path: &Path, expected: &str) -> Result<bool, String> {
    let actual = compute_file_sha256(path)?;
    Ok(actual.eq_ignore_ascii_case(expected))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
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

        // Resume: skip if already complete (and verified when possible).
        if file.size > 0 {
            if let Ok(meta) = tokio::fs::metadata(&dest_path).await {
                if meta.is_file() && meta.len() == file.size {
                    let mut accept = true;
                    if !opts.skip_verify {
                        if let Some(expected) = file.expected_sha256() {
                            let path_for_hash = dest_path.clone();
                            let expected_owned = expected.to_string();
                            let ok = tokio::task::spawn_blocking(move || {
                                verify_file_sha256(&path_for_hash, &expected_owned)
                            })
                            .await
                            .map_err(|e| format!("hash task join error: {e}"))??;
                            if !ok {
                                // Existing file is corrupt — remove and re-download.
                                let _ = tokio::fs::remove_file(&dest_path).await;
                                accept = false;
                            }
                        }
                    }
                    if accept {
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
            let _ = tokio::fs::remove_file(&dest_path).await;
            return Err(format!(
                "size mismatch for {}: expected {}, got {}",
                file.path, file.size, bytes_done
            ));
        }

        // 5. Verify sha256 for LFS files (absent sha256 => skip silently).
        if !opts.skip_verify {
            if let Some(expected) = file.expected_sha256() {
                let path_for_hash = dest_path.clone();
                let expected_owned = expected.to_string();
                let ok = tokio::task::spawn_blocking(move || {
                    verify_file_sha256(&path_for_hash, &expected_owned)
                })
                .await
                .map_err(|e| format!("hash task join error: {e}"))??;
                if !ok {
                    let _ = tokio::fs::remove_file(&dest_path).await;
                    return Err(format!(
                        "sha256 mismatch for {}: expected {}",
                        file.path, expected
                    ));
                }
            }
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
    fn sha256_known_vector() {
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tmp");
        tmp.write_all(b"abc").expect("write");
        tmp.flush().expect("flush");
        let path = tmp.path();
        let hash = compute_file_sha256(path).expect("hash");
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(verify_file_sha256(path, &hash).expect("verify"));
        assert!(verify_file_sha256(
            path,
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
        )
        .expect("verify upper"));
        assert!(!verify_file_sha256(path, &"0".repeat(64)).expect("verify bad"));
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
