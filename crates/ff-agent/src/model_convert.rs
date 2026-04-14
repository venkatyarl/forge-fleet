//! Convert a downloaded safetensors model to MLX format on Apple Silicon.
//!
//! Fallback path for the rare case that the `mlx-community` Hugging Face org
//! does not already ship a pre-converted MLX variant for a given model. In
//! that situation we run the `mlx_lm.convert` CLI locally against a
//! safetensors library entry and register the resulting directory as a new
//! `runtime = "mlx"` row in `fleet_model_library`.
//!
//! Conversion command (see `docs/llm_runtime_reference.md`):
//! ```text
//! mlx_lm.convert --hf-path <source-dir> --mlx-path <dest-dir> --quantize --q-bits <4|8>
//! ```
//!
//! We only ever convert from `runtime = "vllm"` (safetensors) — `mlx` rows are
//! already converted and `llama.cpp` rows are GGUF, which is a completely
//! different conversion pipeline (`convert_hf_to_gguf.py`).

use std::path::{Path, PathBuf};
use std::time::Instant;

use ff_db::{ModelLibraryRow, pg_list_library, pg_upsert_library};
use sqlx::PgPool;
use tokio::process::Command;

/// Options accepted by [`convert_safetensors_to_mlx`].
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// UUID from `fleet_model_library`. The referenced row must have
    /// `runtime = "vllm"` (i.e. a safetensors checkout).
    pub library_id: String,
    /// Quantization bits. Must be 4 or 8.
    pub quant_bits: u8,
    /// Destination directory. Defaults to
    /// `<parent of source>/<slug(catalog_id)>-<quant_bits>bit-mlx`.
    pub output_dir: Option<PathBuf>,
}

/// Result returned on successful conversion.
#[derive(Debug, Clone)]
pub struct ConvertResult {
    /// UUID of the newly-registered MLX library row.
    pub new_library_id: String,
    /// Filesystem path of the converted MLX directory.
    pub output_path: PathBuf,
    /// Wall-clock duration of the conversion in seconds.
    pub duration_seconds: u64,
}

/// Convert a safetensors library entry to MLX.
///
/// Steps:
/// 1. Look up the library row.
/// 2. Refuse unless `runtime == "vllm"`.
/// 3. Refuse unless we are on macOS.
/// 4. Compute / validate the output directory.
/// 5. Spawn `mlx_lm.convert`, streaming stdout+stderr to
///    `~/.forgefleet/logs/convert-<catalog_id>.log`.
/// 6. On success, register a new `runtime = "mlx"` row via
///    [`pg_upsert_library`] and return its UUID.
pub async fn convert_safetensors_to_mlx(
    pool: &PgPool,
    opts: ConvertOptions,
) -> Result<ConvertResult, String> {
    // ── 0. platform check ────────────────────────────────────────────────
    if !cfg!(target_os = "macos") {
        return Err("mlx conversion only runs on macOS (Apple Silicon)".into());
    }
    if !matches!(opts.quant_bits, 4 | 8) {
        return Err(format!(
            "quant_bits must be 4 or 8 (got {})",
            opts.quant_bits
        ));
    }

    // ── 1. look up the library row ───────────────────────────────────────
    let all = pg_list_library(pool, None)
        .await
        .map_err(|e| format!("pg_list_library: {e}"))?;
    let lib: ModelLibraryRow = all
        .into_iter()
        .find(|r| r.id == opts.library_id)
        .ok_or_else(|| format!("library id {} not found", opts.library_id))?;

    // ── 2. runtime check ─────────────────────────────────────────────────
    if lib.runtime != "vllm" {
        return Err(format!(
            "library {} has runtime={:?}; only vllm (safetensors) entries can be converted to mlx",
            lib.id, lib.runtime
        ));
    }

    // ── 3. resolve source directory ──────────────────────────────────────
    let source_path = PathBuf::from(&lib.file_path);
    let source_dir = if source_path.is_dir() {
        source_path.clone()
    } else {
        source_path
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| format!("cannot determine parent of {}", lib.file_path))?
    };
    if !source_dir.exists() {
        return Err(format!(
            "source directory does not exist: {}",
            source_dir.display()
        ));
    }

    // ── 4. compute output dir default ────────────────────────────────────
    let output_dir = match opts.output_dir {
        Some(p) => p,
        None => {
            let parent = source_dir
                .parent()
                .map(PathBuf::from)
                .ok_or_else(|| format!("source dir has no parent: {}", source_dir.display()))?;
            parent.join(format!(
                "{}-{}bit-mlx",
                slug(&lib.catalog_id),
                opts.quant_bits
            ))
        }
    };
    if output_dir.exists() {
        return Err(format!(
            "output directory already exists: {} (remove it explicitly to re-convert)",
            output_dir.display()
        ));
    }

    // Ensure the parent of the output dir exists (mlx_lm.convert will create
    // output_dir itself, but it won't create missing ancestors reliably).
    if let Some(parent) = output_dir.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
    }

    // ── 5. prepare log file ──────────────────────────────────────────────
    let home = dirs::home_dir().ok_or_else(|| "no home dir".to_string())?;
    let log_dir = home.join(".forgefleet/logs");
    std::fs::create_dir_all(&log_dir)
        .map_err(|e| format!("create log dir {}: {e}", log_dir.display()))?;
    let log_path = log_dir.join(format!("convert-{}.log", slug(&lib.catalog_id)));
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open log {}: {e}", log_path.display()))?;
    let log_err = log_file
        .try_clone()
        .map_err(|e| format!("clone log handle: {e}"))?;

    tracing::info!(
        library_id = %lib.id,
        catalog_id = %lib.catalog_id,
        source = %source_dir.display(),
        dest = %output_dir.display(),
        q_bits = opts.quant_bits,
        log = %log_path.display(),
        "starting mlx_lm.convert",
    );

    // ── 6. spawn mlx_lm.convert and wait ─────────────────────────────────
    let started = Instant::now();
    let status = Command::new("mlx_lm.convert")
        .arg("--hf-path")
        .arg(&source_dir)
        .arg("--mlx-path")
        .arg(&output_dir)
        .arg("--quantize")
        .arg("--q-bits")
        .arg(opts.quant_bits.to_string())
        .stdout(log_file)
        .stderr(log_err)
        .stdin(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| format!("spawn mlx_lm.convert: {e} (is mlx-lm installed? `pip install mlx-lm`)"))?;

    if !status.success() {
        return Err(format!(
            "mlx_lm.convert failed (exit={:?}); see log: {}",
            status.code(),
            log_path.display()
        ));
    }
    let duration_seconds = started.elapsed().as_secs();

    // ── 7. register the new MLX library row ──────────────────────────────
    let dir_size = dir_size_bytes(&output_dir).unwrap_or(0);
    let node_name = crate::fleet_info::resolve_this_node_name().await;
    let quant = format!("{}bit", opts.quant_bits);

    let new_library_id = pg_upsert_library(
        pool,
        &node_name,
        &lib.catalog_id,
        "mlx",
        Some(&quant),
        &output_dir.to_string_lossy(),
        dir_size as i64,
        None,
        None,
    )
    .await
    .map_err(|e| format!("pg_upsert_library: {e}"))?;

    tracing::info!(
        new_library_id = %new_library_id,
        duration_seconds,
        size = dir_size,
        "mlx conversion complete",
    );

    Ok(ConvertResult {
        new_library_id,
        output_path: output_dir,
        duration_seconds,
    })
}

/// Make a filesystem-safe slug from a catalog id like
/// `meta-llama/Llama-3.1-8B-Instruct` → `meta-llama-Llama-3.1-8B-Instruct`.
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | ' ' => '-',
            _ => c,
        })
        .collect()
}

/// Recursively total the byte size of every regular file under `dir`.
fn dir_size_bytes(dir: &Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let md = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() {
                stack.push(entry.path());
            } else if md.is_file() {
                total = total.saturating_add(md.len());
            }
        }
    }
    Ok(total)
}
