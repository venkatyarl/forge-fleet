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
    worker_name: &str,
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
    let existing = pg_list_library(pool, Some(worker_name))
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
            let classified = classify_top_level_dir(&path, &catalog);
            if classified.is_empty() {
                if verbose {
                    eprintln!("[scan]  dir   skip: {}", path.display());
                }
            } else {
                for d in classified {
                    if verbose {
                        eprintln!(
                            "[scan]  dir   → catalog={} runtime={} size={}",
                            d.catalog_id, d.runtime, d.size_bytes
                        );
                    }
                    discovered.push(d);
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
            worker_name,
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

/// Classify a top-level directory, descending one level for known directories
/// that group models by runtime/vendor rather than representing a model.
fn classify_top_level_dir(path: &Path, catalog: &[ff_db::ModelCatalogRow]) -> Vec<Discovered> {
    let vendor_runtime = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| {
            if name.eq_ignore_ascii_case("llama-cpp") {
                Some(None)
            } else if name.eq_ignore_ascii_case("mlx") {
                Some(Some("mlx"))
            } else {
                None
            }
        });

    let Some(runtime_hint) = vendor_runtime else {
        return classify_dir(path, catalog).into_iter().collect();
    };

    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|file_type| file_type.is_dir())
                .and_then(|_| classify_dir_with_runtime_hint(&entry.path(), catalog, runtime_hint))
        })
        .collect()
}

/// Classify a single top-level file. Returns `None` if unrecognised.
fn classify_file(path: &Path, catalog: &[ff_db::ModelCatalogRow]) -> Option<Discovered> {
    let name = path.file_name()?.to_string_lossy().to_string();
    if !name.to_lowercase().ends_with(".gguf") {
        return None;
    }

    let size = std::fs::metadata(path).ok().map(|m| m.len()).unwrap_or(0);
    // Strip the `.gguf` extension case-insensitively. The suffix is guaranteed
    // by the check above so this always succeeds; fall back to the full name
    // defensively. (Previously a hand-rolled trim_end_matches('f'/'g'/'g'/'.')
    // chain computed a stem here first — but it dropped the `u`, yielding
    // `model.ggu`, and was always overwritten by this strip_ext. Removed the
    // dead+buggy chain so a future edit can't silently resurrect it.)
    let stem = strip_ext(&name, ".gguf").unwrap_or_else(|| name.clone());

    let quant = extract_gguf_quant(&stem);
    // Base name without the quant suffix (best-effort).
    let base_name = if let Some(q) = &quant {
        stem.trim_end_matches(q)
            .trim_end_matches(['-', '_', '.'])
            .to_string()
    } else {
        stem.clone()
    };

    let catalog_id =
        match_catalog_for_artifact(&base_name, catalog, Some("llama.cpp"), quant.as_deref())
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
    classify_dir_with_runtime_hint(path, catalog, None)
}

/// Classify one model directory, optionally forcing the runtime declared by
/// an explicit vendor parent such as `~/models/mlx`.
///
/// The hint is deliberately structural rather than host-derived: scans and
/// their tests may run on Linux even when the artifact is destined for an
/// Apple-Silicon node. Only the exact `mlx` vendor directory supplies this
/// hint; ordinary HF directories preserve the existing OS/name heuristic.
fn classify_dir_with_runtime_hint(
    path: &Path,
    catalog: &[ff_db::ModelCatalogRow],
    runtime_hint: Option<&str>,
) -> Option<Discovered> {
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
        // Runtime detection:
        //   - explicit mlx hint in dirname  → "mlx" (operator was deliberate)
        //   - on macOS                       → "mlx" (only Apple Silicon runtime
        //                                       that loads safetensors)
        //   - everywhere else                → "vllm" (Linux+CUDA pattern)
        //
        // Before this fix, the scanner picked "vllm" for any safetensors dir
        // regardless of host OS. That caused Vinny's qwen36-35b-a3b
        // (HF-format dir served by mlx_lm.server) to show up as runtime=vllm
        // — which then blocked `ff model delete` with a false "active
        // deployment" check (deployment was on a different runtime).
        let runtime = runtime_hint.map(str::to_owned).unwrap_or_else(|| {
            if lower.ends_with("-mlx")
                || lower.ends_with("-4bit")
                || lower.contains("mlx")
                || std::env::consts::OS == "macos"
            {
                "mlx"
            } else {
                "vllm"
            }
            .to_string()
        });
        if runtime == "mlx"
            && let Err(reason) = validate_mlx_hf_dir(path, &safetensor_paths, has_index)
        {
            tracing::warn!(
                path = %path.display(),
                %reason,
                "skipping incomplete or unsafe MLX model directory"
            );
            return None;
        }
        let quant = extract_hf_quant(&lower);
        let total_size: u64 = safetensor_paths
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
            .sum();
        let catalog_id =
            match_catalog_for_artifact(&dir_name, catalog, Some(&runtime), quant.as_deref())
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
        let catalog_id =
            match_catalog_for_artifact(&dir_name, catalog, Some("llama.cpp"), quant.as_deref())
                .or_else(|| {
                    match_catalog_for_artifact(&stem, catalog, Some("llama.cpp"), quant.as_deref())
                })
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
                if p.is_file()
                    && let Some(ext) = p.extension()
                    && ext.eq_ignore_ascii_case("gguf")
                {
                    nested_ggufs.push(p);
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
        let catalog_id =
            match_catalog_for_artifact(&dir_name, catalog, Some("llama.cpp"), quant.as_deref())
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

/// Validate the minimum local artifact contract needed by `mlx_lm.server`.
///
/// This check is intentionally limited to directories classified as MLX so
/// existing vLLM and llama.cpp scans do not change. Invalid artifacts skip one
/// directory rather than aborting the whole scan. An optional safetensors
/// index is treated as authority when present: every referenced shard must be
/// a non-empty, contained `.safetensors` file. Indexless single-file MLX repos
/// remain valid.
fn validate_mlx_hf_dir(
    path: &Path,
    safetensor_paths: &[PathBuf],
    has_index: bool,
) -> Result<(), String> {
    let config = read_contained_json_object(path, &path.join("config.json"), "config.json")?;
    let has_model_type = config
        .get("model_type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    let has_architecture = config
        .get("architectures")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|values| {
            values
                .iter()
                .any(|value| value.as_str().is_some_and(|value| !value.trim().is_empty()))
        });
    if !has_model_type && !has_architecture {
        return Err("config.json has no model_type or architecture identity".to_string());
    }

    read_contained_json_object(
        path,
        &path.join("tokenizer_config.json"),
        "tokenizer_config.json",
    )?;
    let mut has_tokenizer = false;
    for name in ["tokenizer.json", "tokenizer.model", "spiece.model"] {
        let tokenizer = path.join(name);
        match std::fs::symlink_metadata(&tokenizer) {
            Ok(_) => {
                validate_contained_regular_file(path, &tokenizer, name)?;
                has_tokenizer = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("inspect {name}: {error}")),
        }
    }
    if !has_tokenizer {
        return Err("no non-empty tokenizer artifact found".to_string());
    }

    if safetensor_paths.is_empty() {
        return Err("no safetensors weights found".to_string());
    }
    for weight in safetensor_paths {
        validate_contained_regular_file(path, weight, "safetensors weight")?;
    }

    if has_index {
        validate_safetensors_index(path)?;
    }
    Ok(())
}

fn read_contained_json_object(
    model_root: &Path,
    path: &Path,
    label: &str,
) -> Result<serde_json::Value, String> {
    validate_contained_regular_file(model_root, path, label)?;
    let bytes = std::fs::read(path).map_err(|e| format!("read {label}: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse {label}: {e}"))?;
    if !value.is_object() {
        return Err(format!("{label} is not a JSON object"));
    }
    Ok(value)
}

/// Require a model artifact to be a non-empty regular file whose final path is
/// not a symlink and whose canonical target remains under the canonical model
/// root. `metadata` and `Path::is_file` follow symlinks, so neither is a safe
/// containment check by itself.
fn validate_contained_regular_file(
    model_root: &Path,
    candidate: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    let canonical_root =
        std::fs::canonicalize(model_root).map_err(|e| format!("canonicalize model root: {e}"))?;
    let metadata = std::fs::symlink_metadata(candidate)
        .map_err(|e| format!("inspect {label} {}: {e}", candidate.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{label} is a symlink: {}", candidate.display()));
    }
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return Err(format!(
            "{label} is missing, empty, or not a regular file: {}",
            candidate.display()
        ));
    }
    let canonical_candidate = std::fs::canonicalize(candidate)
        .map_err(|e| format!("canonicalize {label} {}: {e}", candidate.display()))?;
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(format!(
            "{label} escapes model directory: {}",
            candidate.display()
        ));
    }
    Ok(canonical_candidate)
}

fn validate_safetensors_index(model_root: &Path) -> Result<(), String> {
    use std::path::Component;

    let index = read_contained_json_object(
        model_root,
        &model_root.join("model.safetensors.index.json"),
        "model.safetensors.index.json",
    )?;
    let weight_map = index
        .get("weight_map")
        .and_then(serde_json::Value::as_object)
        .filter(|map| !map.is_empty())
        .ok_or_else(|| "safetensors index has no non-empty weight_map".to_string())?;
    let mut shards = std::collections::BTreeSet::new();
    for value in weight_map.values() {
        let relative = value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "safetensors index contains an empty shard path".to_string())?;
        let relative_path = Path::new(relative);
        if relative_path.is_absolute()
            || relative_path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(format!("unsafe safetensors shard path: {relative}"));
        }
        if !relative_path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("safetensors"))
        {
            return Err(format!(
                "index references a non-safetensors file: {relative}"
            ));
        }
        shards.insert(relative.to_string());
    }

    for relative in shards {
        let shard = model_root.join(&relative);
        validate_contained_regular_file(model_root, &shard, "indexed shard")?;
    }
    Ok(())
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
    // Rank by rightmost END position, tie-breaking on the LONGER tag. Ranking
    // by start index alone misclassifies overlapping tags: "F16" is a substring
    // of "BF16" (`B F 1 6`), so its rfind start sits one byte to the right and
    // would wrongly win — tagging a bf16 model as f16. Both share the same end
    // index, so the length tie-break correctly prefers "BF16". A genuinely
    // later tag (e.g. "…Q4_K_M…Q8_0") still wins on end position.
    let mut best: Option<(usize, usize, &str)> = None; // (end_idx, len, tag)
    for c in candidates {
        if let Some(idx) = upper.rfind(c) {
            let key = (idx + c.len(), c.len());
            if best.map(|(e, l, _)| key > (e, l)).unwrap_or(true) {
                best = Some((key.0, key.1, c));
            }
        }
    }
    best.map(|(_, _, c)| c.to_string())
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

#[cfg(test)]
fn match_catalog(needle: &str, catalog: &[ff_db::ModelCatalogRow]) -> Option<String> {
    match_catalog_for_artifact(needle, catalog, None, None)
}

/// Case-insensitive identity match against catalog `id` and `name`.
///
/// Exact canonical identity wins before substring specificity. That ordering is
/// important for format-specific scout rows: `qwen3-6-35b-a3b-nvfp4` is a
/// longer substring match for a `qwen3.6-35b` directory, but it is not the
/// identity of a Q4_K_M GGUF stored there. Artifact hints break ties between
/// canonically-equivalent ids using the catalog's declared runtime/quant.
fn match_catalog_for_artifact(
    needle: &str,
    catalog: &[ff_db::ModelCatalogRow],
    runtime: Option<&str>,
    quant: Option<&str>,
) -> Option<String> {
    // Canonical form: lowercase + strip punctuation so "gemma-4" and "gemma4" match.
    let canon = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let n = canon(needle);
    // An empty needle (e.g. a filename that canonicalises to nothing) must not
    // match anything: `id_c.contains("")` is always true, so without this guard
    // every catalog row would "match" and the longest id would be returned.
    if n.is_empty() {
        return None;
    }
    // Substring match in either direction, but an EMPTY pattern is never a
    // match — `"".contains(...)`/`n.contains("")` would otherwise let a catalog
    // row with a punctuation-only id or name match every needle.
    let contains_either = |p: &str| -> bool { !p.is_empty() && (n.contains(p) || p.contains(&n)) };
    // (identity rank, artifact compatibility, substring specificity, id)
    // Lexicographic tuple ordering keeps the result deterministic without
    // allowing a longer format suffix to outrank an exact base-model identity.
    let mut best: Option<((u8, u8, usize), String)> = None;
    for row in catalog {
        let id_c = canon(&row.id);
        let name_c = canon(&row.name);
        // Direct-contains match (either direction)
        let hit = contains_either(&id_c) || contains_either(&name_c);
        if !hit || identity_declares_conflicting_quant(row, quant) {
            continue;
        }

        let identity_rank = if id_c == n {
            3
        } else if name_c == n {
            2
        } else {
            1
        };
        let compatibility = artifact_compatibility(row, runtime, quant);
        let score = (identity_rank, compatibility, id_c.len());
        if best
            .as_ref()
            .map(|(current, current_id)| {
                score > *current || (score == *current && row.id < *current_id)
            })
            .unwrap_or(true)
        {
            best = Some((score, row.id.clone()));
        }
    }
    best.map(|(_, id)| id)
}

/// Reject a format-qualified catalog identity when the on-disk quant proves a
/// different format. This is intentionally limited to suffixes embedded in the
/// row identity; a catalog row may legitimately expose several variants with
/// different quants, so variant metadata itself is used for ranking, not
/// blanket rejection.
fn identity_declares_conflicting_quant(
    row: &ff_db::ModelCatalogRow,
    artifact_quant: Option<&str>,
) -> bool {
    let Some(artifact_quant) = artifact_quant else {
        return false;
    };
    let canonical_quant = artifact_quant
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let identity = format!("{} {}", row.id, row.name).to_ascii_lowercase();

    ["nvfp4", "fp4", "fp8", "bf16", "fp16"]
        .into_iter()
        .find(|marker| identity.contains(marker))
        .is_some_and(|declared| canonical_quant != declared)
}

/// Rank a catalog row by how specifically one of its declared variants matches
/// the discovered artifact. Missing metadata is neutral; it never beats an
/// explicit runtime+quant match.
fn artifact_compatibility(
    row: &ff_db::ModelCatalogRow,
    runtime: Option<&str>,
    quant: Option<&str>,
) -> u8 {
    let normalize = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let runtime = runtime.map(normalize);
    let quant = quant.map(normalize);

    row.variants
        .as_array()
        .into_iter()
        .flatten()
        .map(|variant| {
            let runtime_match = runtime.as_ref().is_some_and(|wanted| {
                variant
                    .get("runtime")
                    .and_then(|value| value.as_str())
                    .is_some_and(|value| normalize(value) == *wanted)
            });
            let quant_match = quant.as_ref().is_some_and(|wanted| {
                variant
                    .get("quant")
                    .and_then(|value| value.as_str())
                    .is_some_and(|value| normalize(value) == *wanted)
            });
            u8::from(runtime_match) + 2 * u8::from(quant_match)
        })
        .max()
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gguf_quant_does_not_confuse_bf16_with_f16() {
        // Regression: "F16" is a substring of "BF16", so a start-index ranking
        // tagged a bf16 model as f16. The end-position + length tie-break must
        // keep BF16 distinct.
        assert_eq!(
            extract_gguf_quant("Meta-Llama-3-8B.BF16"),
            Some("BF16".to_string())
        );
        assert_eq!(extract_gguf_quant("model-F16"), Some("F16".to_string()));
        assert_eq!(extract_gguf_quant("model-FP16"), Some("FP16".to_string()));
    }

    #[test]
    fn gguf_quant_picks_common_llamacpp_tags() {
        assert_eq!(
            extract_gguf_quant("qwen2.5-coder-7b-instruct-q4_k_m"),
            Some("Q4_K_M".to_string())
        );
        assert_eq!(
            extract_gguf_quant("some-model.Q8_0"),
            Some("Q8_0".to_string())
        );
        assert_eq!(extract_gguf_quant("plain-model-name"), None);
    }

    #[test]
    fn gguf_quant_prefers_the_rightmost_tag() {
        // When two genuine tags appear, the one nearest the extension (the
        // actual quant) wins by end position.
        assert_eq!(
            extract_gguf_quant("merged-Q4_0-then-final-Q8_0"),
            Some("Q8_0".to_string())
        );
    }

    fn row(id: &str, name: &str) -> ff_db::ModelCatalogRow {
        ff_db::ModelCatalogRow {
            id: id.to_string(),
            name: name.to_string(),
            family: String::new(),
            parameters: String::new(),
            tier: 0,
            description: None,
            gated: false,
            preferred_workloads: serde_json::json!([]),
            variants: serde_json::json!([]),
            tool_calling: false,
        }
    }

    fn row_with_variants(
        id: &str,
        name: &str,
        variants: serde_json::Value,
    ) -> ff_db::ModelCatalogRow {
        let mut row = row(id, name);
        row.variants = variants;
        row
    }

    fn write_valid_mlx_model(path: &Path, with_index: bool) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("config.json"),
            br#"{"model_type":"qwen3","architectures":["Qwen3ForCausalLM"]}"#,
        )
        .unwrap();
        std::fs::write(
            path.join("tokenizer_config.json"),
            br#"{"tokenizer_class":"Qwen2Tokenizer"}"#,
        )
        .unwrap();
        std::fs::write(path.join("tokenizer.json"), br#"{"version":"1.0"}"#).unwrap();
        std::fs::write(path.join("model.safetensors"), b"weights").unwrap();
        if with_index {
            std::fs::write(
                path.join("model.safetensors.index.json"),
                br#"{"weight_map":{"model.embed_tokens.weight":"model.safetensors"}}"#,
            )
            .unwrap();
        }
    }

    #[test]
    fn match_catalog_prefers_longer_id() {
        let cat = vec![
            row("qwen3", "Qwen3"),
            row("qwen3-coder-30b", "Qwen3 Coder 30B"),
        ];
        // A discovered name that contains both ids resolves to the more
        // specific (longer) catalog id.
        assert_eq!(
            match_catalog("qwen3-coder-30b-instruct", &cat),
            Some("qwen3-coder-30b".to_string())
        );
    }

    #[test]
    fn exact_identity_outranks_longer_format_suffix() {
        let cat = vec![
            row("qwen36-35b-a3b", "Qwen3.6-35B-A3B"),
            row("qwen3-6-35b-a3b-nvfp4", "Qwen3.6-35B-A3B-NVFP4"),
        ];
        assert_eq!(
            match_catalog("qwen3.6-35b-a3b", &cat),
            Some("qwen36-35b-a3b".to_string())
        );
    }

    #[test]
    fn artifact_metadata_breaks_canonical_id_tie() {
        let cat = vec![
            row("qwen3-6-35b-a3b", "Qwen3.6-35B-A3B"),
            row_with_variants(
                "qwen36-35b-a3b",
                "Qwen3.6-35B-A3B-Instruct",
                serde_json::json!([{
                    "runtime": "llama.cpp",
                    "quant": "Q4_K_M",
                    "hf_repo": "Qwen/Qwen3.6-35B-A3B-Instruct-GGUF"
                }]),
            ),
        ];
        assert_eq!(
            match_catalog_for_artifact("qwen3.6-35b-a3b", &cat, Some("llama.cpp"), Some("Q4_K_M")),
            Some("qwen36-35b-a3b".to_string())
        );
    }

    #[test]
    fn incompatible_format_qualified_identity_is_rejected() {
        let cat = vec![row("qwen3-6-35b-a3b-nvfp4", "Qwen3.6-35B-A3B-NVFP4")];
        assert_eq!(
            match_catalog_for_artifact("qwen3.6-35b-a3b", &cat, Some("llama.cpp"), Some("Q4_K_M")),
            None
        );
    }

    #[test]
    fn nested_precision_marker_does_not_conflict_with_itself() {
        let nvfp4 = row("qwen3-6-35b-a3b-nvfp4", "Qwen3.6-35B-A3B-NVFP4");
        assert!(!identity_declares_conflicting_quant(&nvfp4, Some("NVFP4")));
    }

    #[test]
    fn qwen36_q4_directory_resolves_to_curated_runtime_variant() {
        let temp = tempfile::tempdir().unwrap();
        let model = temp.path().join("qwen3.6-35b");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("Qwen3.6-35B-A3B-UD-Q4_K_M.gguf"), b"gguf").unwrap();

        let catalog = vec![
            row("qwen3-6-35b-a3b", "Qwen3.6-35B-A3B"),
            row_with_variants(
                "qwen36-35b-a3b",
                "Qwen3.6-35B-A3B-Instruct",
                serde_json::json!([{
                    "runtime": "llama.cpp",
                    "quant": "Q4_K_M"
                }]),
            ),
            row("qwen3-6-35b-a3b-nvfp4", "Qwen3.6-35B-A3B-NVFP4"),
        ];

        let discovered = classify_dir(&model, &catalog).expect("GGUF directory classified");
        assert_eq!(discovered.catalog_id, "qwen36-35b-a3b");
        assert_eq!(discovered.runtime, "llama.cpp");
        assert_eq!(discovered.quant.as_deref(), Some("Q4_K_M"));
    }

    #[test]
    fn match_catalog_empty_needle_matches_nothing() {
        // Regression: an empty needle used to match every row via
        // `id_c.contains("")` and return the longest id.
        let cat = vec![
            row("qwen3-coder-30b", "Qwen3 Coder 30B"),
            row("llama-3-8b", "Llama 3 8B"),
        ];
        assert_eq!(match_catalog("---", &cat), None);
        assert_eq!(match_catalog("", &cat), None);
    }

    #[test]
    fn match_catalog_empty_catalog_pattern_matches_nothing() {
        // Regression: a catalog row whose id+name canonicalise to empty used to
        // match every needle via `n.contains("")`.
        let cat = vec![row("", "")];
        assert_eq!(match_catalog("totally-unrelated-model-xyz", &cat), None);
    }

    #[test]
    fn match_catalog_no_match_returns_none() {
        let cat = vec![row("qwen3-coder-30b", "Qwen3 Coder 30B")];
        assert_eq!(match_catalog("mistral-7b-instruct", &cat), None);
    }

    #[test]
    fn llama_cpp_vendor_dir_classifies_child_models_by_directory_name() {
        let temp = tempfile::tempdir().unwrap();
        let vendor = temp.path().join("llama-cpp");
        let model = vendor.join("qwen3-coder-480b");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("model.gguf"), b"gguf").unwrap();

        let discovered =
            classify_top_level_dir(&vendor, &[row("qwen3-coder-480b", "Qwen3 Coder 480B")]);

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].catalog_id, "qwen3-coder-480b");
        assert_eq!(discovered[0].file_path, model.to_string_lossy());
        assert_eq!(discovered[0].runtime, "llama.cpp");
    }

    #[test]
    fn mlx_vendor_dir_forces_runtime_and_matches_ace_catalog_identity() {
        let temp = tempfile::tempdir().unwrap();
        let vendor = temp.path().join("mlx");
        let model = vendor.join("qwen3-4b-instruct-2507-4bit-7494131");
        write_valid_mlx_model(&model, true);
        let catalog = vec![row_with_variants(
            "qwen3-4b-instruct-2507",
            "Qwen3-4B-Instruct-2507",
            serde_json::json!([{
                "runtime": "llama.cpp",
                "quant": "Q4_K_M"
            }]),
        )];

        let discovered = classify_top_level_dir(&vendor, &catalog);

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].catalog_id, "qwen3-4b-instruct-2507");
        assert_eq!(discovered[0].runtime, "mlx");
        assert_eq!(discovered[0].quant.as_deref(), Some("4bit"));
        assert_eq!(discovered[0].file_path, model.to_string_lossy());
        assert_eq!(discovered[0].size_bytes, 7);
    }

    #[test]
    fn mlx_vendor_runtime_hint_does_not_depend_on_child_name_or_build_os() {
        let temp = tempfile::tempdir().unwrap();
        let vendor = temp.path().join("MLX");
        let model = vendor.join("qwen3-4b-instruct-2507");
        write_valid_mlx_model(&model, false);

        let discovered = classify_top_level_dir(
            &vendor,
            &[row("qwen3-4b-instruct-2507", "Qwen3-4B-Instruct-2507")],
        );

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].runtime, "mlx");
        assert_eq!(discovered[0].quant, None);
    }

    #[test]
    fn mlx_indexless_single_file_repo_is_valid() {
        let temp = tempfile::tempdir().unwrap();
        let model = temp.path().join("model-4bit");
        write_valid_mlx_model(&model, false);

        let discovered = classify_dir_with_runtime_hint(&model, &[], Some("mlx"));

        assert!(discovered.is_some());
        assert_eq!(discovered.unwrap().size_bytes, 7);
    }

    #[test]
    fn mlx_invalid_config_or_missing_tokenizer_is_skipped_per_directory() {
        let temp = tempfile::tempdir().unwrap();
        let malformed = temp.path().join("malformed");
        write_valid_mlx_model(&malformed, false);
        std::fs::write(malformed.join("config.json"), b"not-json").unwrap();
        assert!(classify_dir_with_runtime_hint(&malformed, &[], Some("mlx")).is_none());

        let no_tokenizer = temp.path().join("no-tokenizer");
        write_valid_mlx_model(&no_tokenizer, false);
        std::fs::remove_file(no_tokenizer.join("tokenizer.json")).unwrap();
        assert!(classify_dir_with_runtime_hint(&no_tokenizer, &[], Some("mlx")).is_none());

        let valid = temp.path().join("still-valid");
        write_valid_mlx_model(&valid, false);
        assert!(classify_dir_with_runtime_hint(&valid, &[], Some("mlx")).is_some());
    }

    #[test]
    fn mlx_index_rejects_traversal_missing_and_empty_shards() {
        let temp = tempfile::tempdir().unwrap();

        let traversal = temp.path().join("traversal");
        write_valid_mlx_model(&traversal, true);
        std::fs::write(
            traversal.join("model.safetensors.index.json"),
            br#"{"weight_map":{"x":"../outside.safetensors"}}"#,
        )
        .unwrap();
        assert!(classify_dir_with_runtime_hint(&traversal, &[], Some("mlx")).is_none());

        let missing = temp.path().join("missing");
        write_valid_mlx_model(&missing, true);
        std::fs::write(
            missing.join("model.safetensors.index.json"),
            br#"{"weight_map":{"x":"missing.safetensors"}}"#,
        )
        .unwrap();
        assert!(classify_dir_with_runtime_hint(&missing, &[], Some("mlx")).is_none());

        let empty = temp.path().join("empty");
        write_valid_mlx_model(&empty, true);
        std::fs::write(empty.join("model.safetensors"), b"").unwrap();
        assert!(classify_dir_with_runtime_hint(&empty, &[], Some("mlx")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn mlx_index_rejects_a_symlinked_shard_that_escapes_the_model_dir() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let model = temp.path().join("model");
        write_valid_mlx_model(&model, true);
        let outside = temp.path().join("outside.safetensors");
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::remove_file(model.join("model.safetensors")).unwrap();
        symlink(&outside, model.join("model.safetensors")).unwrap();

        assert!(classify_dir_with_runtime_hint(&model, &[], Some("mlx")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn mlx_indexless_repo_rejects_a_symlinked_weight_that_escapes_the_model_dir() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let model = temp.path().join("model");
        write_valid_mlx_model(&model, false);
        let outside = temp.path().join("outside.safetensors");
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::remove_file(model.join("model.safetensors")).unwrap();
        symlink(&outside, model.join("model.safetensors")).unwrap();

        assert!(classify_dir_with_runtime_hint(&model, &[], Some("mlx")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn mlx_rejects_symlinked_config_and_tokenizer_artifacts() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        for (artifact, outside_contents) in [
            ("config.json", br#"{"model_type":"qwen3"}"#.as_slice()),
            (
                "tokenizer_config.json",
                br#"{"tokenizer_class":"Qwen2Tokenizer"}"#.as_slice(),
            ),
            ("tokenizer.json", br#"{"version":"1.0"}"#.as_slice()),
        ] {
            let model = temp
                .path()
                .join(format!("model-{}", artifact.replace('.', "-")));
            write_valid_mlx_model(&model, false);
            let outside = temp.path().join(format!("outside-{artifact}"));
            std::fs::write(&outside, outside_contents).unwrap();
            std::fs::remove_file(model.join(artifact)).unwrap();
            symlink(&outside, model.join(artifact)).unwrap();

            assert!(
                classify_dir_with_runtime_hint(&model, &[], Some("mlx")).is_none(),
                "symlinked {artifact} must be rejected"
            );
        }
    }
}
