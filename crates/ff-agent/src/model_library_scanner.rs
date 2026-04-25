//! Scans a local models directory and reconciles it with `fleet_model_library`.
//!
//! Walks `models_dir` (non-recursive at top-level) and recognises:
//!   - Single `*.gguf` files                           → runtime = "llama.cpp"
//!   - HF-style safetensors directories                → runtime = "vllm" / "mlx"
//!
//! For each discovered entry it calls [`ff_db::pg_upsert_library`] and, at the
//! end, removes any library rows whose `file_path` is no longer on disk.

use std::path::{Path, PathBuf};

use ff_db::{pg_delete_library, pg_list_catalog, pg_list_library, pg_upsert_library};

/// Summary of a scan run.
#[derive(Debug, Clone, Default)]
pub struct ScanSummary {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub total_bytes: u64,
}

/// Classified entry discovered on disk.
#[derive(Debug, Clone)]
struct Discovered {
    catalog_id: String,
    runtime: String,
    quant: Option<String>,
    file_path: String,
    size_bytes: u64,
}

/// Scan `models_dir` and reconcile against Postgres.
pub async fn scan_local_library(
    pool: &sqlx::PgPool,
    node_name: &str,
    models_dir: &Path,
) -> Result<ScanSummary, String> {
    if !models_dir.exists() {
        return Err(format!(
            "models_dir does not exist: {}",
            models_dir.display()
        ));
    }
    if !models_dir.is_dir() {
        return Err(format!(
            "models_dir is not a directory: {}",
            models_dir.display()
        ));
    }

    // Fetch catalog once for fuzzy matching.
    let catalog = pg_list_catalog(pool)
        .await
        .map_err(|e| format!("pg_list_catalog failed: {e}"))?;

    // Existing library rows for this node — used for removal reconciliation &
    // distinguishing added vs updated.
    let existing = pg_list_library(pool, Some(node_name))
        .await
        .map_err(|e| format!("pg_list_library failed: {e}"))?;

    // Walk the directory (non-recursive at top level).
    let entries = std::fs::read_dir(models_dir)
        .map_err(|e| format!("read_dir({}) failed: {e}", models_dir.display()))?;

    let verbose = std::env::var("FORGEFLEET_SCAN_DEBUG").ok().as_deref() == Some("1");
    let mut discovered: Vec<Discovered> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("read_dir entry error: {e}");
                if verbose {
                    eprintln!("[scan] read_dir entry error: {e}");
                }
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                tracing::warn!("file_type({}) failed: {e}", path.display());
                if verbose {
                    eprintln!("[scan] file_type({}) failed: {e}", path.display());
                }
                continue;
            }
        };

        if file_type.is_file() {
            match classify_file(&path, &catalog) {
                Some(d) => {
                    if verbose {
                        eprintln!(
                            "[scan]  file  → catalog={} runtime={} size={}",
                            d.catalog_id, d.runtime, d.size_bytes
                        );
                    }
                    discovered.push(d);
                }
                None => {
                    if verbose {
                        eprintln!("[scan]  file  skip: {}", path.display());
                    }
                }
            }
        } else if file_type.is_dir() {
            match classify_dir(&path, &catalog) {
                Some(d) => {
                    if verbose {
                        eprintln!(
                            "[scan]  dir   → catalog={} runtime={} size={}",
                            d.catalog_id, d.runtime, d.size_bytes
                        );
                    }
                    discovered.push(d);
                }
                None => {
                    if verbose {
                        eprintln!("[scan]  dir   skip: {}", path.display());
                    }
                }
            }
        } else if verbose {
            eprintln!("[scan]  other skip: {}", path.display());
        }
    }
    if verbose {
        eprintln!("[scan] discovered {} entries total", discovered.len());
    }

    // Index existing rows by file_path for added/updated bookkeeping.
    let existing_paths: std::collections::HashSet<String> =
        existing.iter().map(|r| r.file_path.clone()).collect();

    let mut summary = ScanSummary::default();
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    for d in &discovered {
        let was_present = existing_paths.contains(&d.file_path);
        match pg_upsert_library(
            pool,
            node_name,
            &d.catalog_id,
            &d.runtime,
            d.quant.as_deref(),
            &d.file_path,
            d.size_bytes as i64,
            None,
            None,
        )
        .await
        {
            Ok(_) => {
                if was_present {
                    summary.updated += 1;
                } else {
                    summary.added += 1;
                }
                summary.total_bytes = summary.total_bytes.saturating_add(d.size_bytes);
                seen_paths.insert(d.file_path.clone());
            }
            Err(e) => {
                tracing::error!("pg_upsert_library failed for {}: {e}", d.file_path);
                if verbose {
                    eprintln!("[scan] UPSERT FAILED for {}: {e}", d.file_path);
                }
            }
        }
    }

    // Remove any rows whose file_path is no longer present on disk.
    for row in &existing {
        if seen_paths.contains(&row.file_path) {
            continue;
        }
        let still_exists = Path::new(&row.file_path).exists();
        if !still_exists {
            match pg_delete_library(pool, &row.id).await {
                Ok(true) => summary.removed += 1,
                Ok(false) => {}
                Err(e) => tracing::error!("pg_delete_library({}) failed: {e}", row.id),
            }
        }
    }

    Ok(summary)
}

/// Classify a single top-level file. Returns `None` if unrecognised.
fn classify_file(path: &Path, catalog: &[ff_db::ModelCatalogRow]) -> Option<Discovered> {
    let name = path.file_name()?.to_string_lossy().to_string();
    if !name.to_lowercase().ends_with(".gguf") {
        return None;
    }

    let size = std::fs::metadata(path).ok().map(|m| m.len()).unwrap_or(0);
    let stem = name
        .trim_end_matches(|c: char| c == 'f' || c == 'F')
        .trim_end_matches(|c: char| c == 'g' || c == 'G')
        .trim_end_matches(|c: char| c == 'g' || c == 'G')
        .trim_end_matches('.')
        .to_string();
    // Simpler: strip .gguf extension case-insensitively.
    let stem = strip_ext(&name, ".gguf").unwrap_or(stem);

    let quant = extract_gguf_quant(&stem);
    // Base name without the quant suffix (best-effort).
    let base_name = if let Some(q) = &quant {
        stem.trim_end_matches(q)
            .trim_end_matches(|c: char| c == '-' || c == '_' || c == '.')
            .to_string()
    } else {
        stem.clone()
    };

    let catalog_id = match_catalog(&base_name, catalog)
        .unwrap_or_else(|| format!("unknown:{}", slugify(&base_name)));

    Some(Discovered {
        catalog_id,
        runtime: "llama.cpp".to_string(),
        quant,
        file_path: path.to_string_lossy().to_string(),
        size_bytes: size,
    })
}

/// Classify a directory. Recognises three layouts:
///   1. HF-style safetensors dir (model.safetensors.index.json + shards)
///   2. Single/multi GGUF file(s) inside a dir
///   3. Nested subdirectory with GGUF shards (e.g. qwen3-235b-q4km/Q4_K_M/*.gguf)
///
/// Returns `None` for unrecognised directories.
fn classify_dir(path: &Path, catalog: &[ff_db::ModelCatalogRow]) -> Option<Discovered> {
    let dir_name = path.file_name()?.to_string_lossy().to_string();
    let lower = dir_name.to_lowercase();

    // Layout 1: HF safetensors (index or shards at top level).
    let has_index = path.join("model.safetensors.index.json").is_file();
    let mut safetensor_paths: Vec<PathBuf> = Vec::new();
    let mut gguf_paths: Vec<PathBuf> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                if let Some(ext) = p.extension() {
                    if ext.eq_ignore_ascii_case("safetensors") {
                        safetensor_paths.push(p);
                    } else if ext.eq_ignore_ascii_case("gguf") {
                        gguf_paths.push(p);
                    }
                }
            } else if p.is_dir() {
                subdirs.push(p);
            }
        }
    }

    // --- Safetensors path ---
    if has_index || !safetensor_paths.is_empty() {
        let runtime =
            if lower.ends_with("-mlx") || lower.ends_with("-4bit") || lower.contains("mlx") {
                "mlx"
            } else {
                "vllm"
            }
            .to_string();
        let quant = extract_hf_quant(&lower);
        let total_size: u64 = safetensor_paths
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
            .sum();
        let catalog_id = match_catalog(&dir_name, catalog)
            .unwrap_or_else(|| format!("unknown:{}", slugify(&dir_name)));
        return Some(Discovered {
            catalog_id,
            runtime,
            quant,
            file_path: path.to_string_lossy().to_string(),
            size_bytes: total_size,
        });
    }

    // --- GGUF-in-dir path (top-level .gguf files) ---
    if !gguf_paths.is_empty() {
        // Pick the first as canonical; size = sum of all ggufs in this dir.
        let total_size: u64 = gguf_paths
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
            .sum();
        let first = &gguf_paths[0];
        let first_name = first
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let stem = strip_ext(&first_name, ".gguf").unwrap_or_else(|| first_name.clone());
        let quant = extract_gguf_quant(&stem).or_else(|| extract_gguf_quant(&dir_name));
        let catalog_id = match_catalog(&dir_name, catalog)
            .or_else(|| match_catalog(&stem, catalog))
            .unwrap_or_else(|| format!("unknown:{}", slugify(&dir_name)));
        return Some(Discovered {
            catalog_id,
            runtime: "llama.cpp".to_string(),
            quant,
            file_path: path.to_string_lossy().to_string(),
            size_bytes: total_size,
        });
    }

    // --- Nested GGUF subdirectory (e.g. qwen3-235b-q4km/Q4_K_M/*.gguf) ---
    let mut nested_ggufs: Vec<PathBuf> = Vec::new();
    for sd in &subdirs {
        if let Ok(rd) = std::fs::read_dir(sd) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_file() {
                    if let Some(ext) = p.extension() {
                        if ext.eq_ignore_ascii_case("gguf") {
                            nested_ggufs.push(p);
                        }
                    }
                }
            }
        }
    }
    if !nested_ggufs.is_empty() {
        let total_size: u64 = nested_ggufs
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
            .sum();
        // Use parent subdirectory name as quant hint if present.
        let quant = nested_ggufs[0]
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .and_then(extract_gguf_quant)
            .or_else(|| extract_gguf_quant(&dir_name));
        let catalog_id = match_catalog(&dir_name, catalog)
            .unwrap_or_else(|| format!("unknown:{}", slugify(&dir_name)));
        return Some(Discovered {
            catalog_id,
            runtime: "llama.cpp".to_string(),
            quant,
            file_path: path.to_string_lossy().to_string(),
            size_bytes: total_size,
        });
    }

    None
}

/// Strip a case-insensitive extension from a filename.
fn strip_ext(name: &str, ext: &str) -> Option<String> {
    let n = name.to_lowercase();
    let e = ext.to_lowercase();
    if n.ends_with(&e) {
        Some(name[..name.len() - ext.len()].to_string())
    } else {
        None
    }
}

/// Pull a llama.cpp quant tag (e.g. `Q4_K_M`, `Q8_0`, `F16`) out of a filename stem.
fn extract_gguf_quant(stem: &str) -> Option<String> {
    let upper = stem.to_uppercase();
    // Common quant patterns — search the tail of the stem.
    let candidates = [
        "Q2_K", "Q3_K_S", "Q3_K_M", "Q3_K_L", "Q4_0", "Q4_1", "Q4_K_S", "Q4_K_M", "Q5_0", "Q5_1",
        "Q5_K_S", "Q5_K_M", "Q6_K", "Q8_0", "F16", "FP16", "BF16", "F32",
    ];
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        if let Some(idx) = upper.rfind(c) {
            // Prefer the rightmost match.
            if best.map(|(i, _)| idx > i).unwrap_or(true) {
                best = Some((idx, c));
            }
        }
    }
    best.map(|(_, c)| c.to_string())
}

/// Pull a quant hint from an HF directory name ("4bit", "8bit", "fp16").
fn extract_hf_quant(lower: &str) -> Option<String> {
    if lower.contains("4bit") {
        Some("4bit".to_string())
    } else if lower.contains("8bit") {
        Some("8bit".to_string())
    } else if lower.contains("fp16") {
        Some("fp16".to_string())
    } else if lower.contains("bf16") {
        Some("bf16".to_string())
    } else {
        None
    }
}

/// Case-insensitive substring match against catalog `id` and `name`.
/// Returns the best (longest match) catalog id.
fn match_catalog(needle: &str, catalog: &[ff_db::ModelCatalogRow]) -> Option<String> {
    // Canonical form: lowercase + strip punctuation so "gemma-4" and "gemma4" match.
    let canon = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let n = canon(needle);
    let mut best: Option<(usize, String)> = None;
    for row in catalog {
        let id_c = canon(&row.id);
        let name_c = canon(&row.name);
        // Direct-contains match (either direction)
        let hit =
            n.contains(&id_c) || id_c.contains(&n) || n.contains(&name_c) || name_c.contains(&n);
        if hit {
            // Prefer longer catalog-id match — "qwen3-coder-30b" over "qwen3-14b" when both hit.
            let score = id_c.len();
            if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                best = Some((score, row.id.clone()));
            }
        }
    }
    best.map(|(_, id)| id)
}

/// Lowercase, replace non-alphanumeric with `-`, collapse repeats.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}
