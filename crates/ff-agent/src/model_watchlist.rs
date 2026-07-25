//! Autopilot-5: materialize one cold fleet copy of watchlisted models.

use std::path::{Path, PathBuf};

use ff_pulse::{beat_v2::PulseBeatV2, reader::PulseReader};
use serde::Deserialize;
use sqlx::{PgPool, Row};

const DEFAULT_BUILD_RESERVE_GB: f64 = 5.0;

#[derive(Debug, Clone, Deserialize)]
struct Variant {
    runtime: String,
    quant: String,
    hf_repo: String,
    size_gb: f64,
    #[serde(default)]
    allow_patterns: Vec<String>,
}

#[derive(Debug)]
struct Candidate {
    id: String,
    gated: bool,
    license: Option<String>,
    variants: serde_json::Value,
}

fn permissive_license(license: Option<&str>) -> bool {
    matches!(
        license
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("apache-2.0" | "mit" | "bsd-2-clause" | "bsd-3-clause" | "isc")
    )
}

fn choose_variant(value: &serde_json::Value) -> Option<Variant> {
    let mut variants: Vec<Variant> = serde_json::from_value(value.clone()).ok()?;
    variants.retain(|v| {
        v.runtime == "llama.cpp"
            && !v.hf_repo.trim().is_empty()
            && v.size_gb.is_finite()
            && v.size_gb > 0.0
    });
    variants.sort_by(|a, b| {
        a.size_gb
            .total_cmp(&b.size_gb)
            .then_with(|| a.quant.cmp(&b.quant))
            .then_with(|| a.hf_repo.cmp(&b.hf_repo))
    });
    variants.into_iter().next()
}

fn node_fits(beat: &PulseBeatV2, required_gb: f64) -> bool {
    !beat.going_offline
        && !beat.maintenance_mode
        && beat.load.disk_free_gb >= required_gb
        && beat.memory.ram_available_for_new_llm_gb >= required_gb
}

fn choose_node(beats: &[PulseBeatV2], required_gb: f64) -> Option<&str> {
    beats
        .iter()
        .filter(|beat| node_fits(beat, required_gb))
        .max_by(|a, b| {
            let a_headroom = a.load.disk_free_gb + a.memory.ram_available_for_new_llm_gb;
            let b_headroom = b.load.disk_free_gb + b.memory.ram_available_for_new_llm_gb;
            a_headroom
                .total_cmp(&b_headroom)
                .then_with(|| b.computer_name.cmp(&a.computer_name))
        })
        .map(|beat| beat.computer_name.as_str())
}

fn expand_models_dir(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => directory_size(&path),
                Ok(kind) if kind.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0,
            }
        })
        .sum()
}

/// Run one local watchlist pass. Every node computes the same winner; only that
/// node acquires the download lock and writes to its own models directory.
pub async fn reconcile(pool: &PgPool, worker_name: &str) -> Result<usize, String> {
    let rows = sqlx::query(
        "SELECT c.id, c.gated, c.license, c.variants
           FROM fleet_model_catalog c
          WHERE c.watchlist
            AND NOT EXISTS (
                SELECT 1 FROM fleet_model_library l WHERE l.catalog_id = c.id
            )
          ORDER BY c.tier, c.id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("list watchlist: {e}"))?;

    let candidates: Vec<Candidate> = rows
        .into_iter()
        .map(|row| Candidate {
            id: row.get("id"),
            gated: row.get("gated"),
            license: row.get("license"),
            variants: row.get("variants"),
        })
        .collect();
    if candidates.is_empty() {
        return Ok(0);
    }

    let redis_url =
        std::env::var("FORGEFLEET_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:56379".into());
    let pulse = PulseReader::new(&redis_url).map_err(|e| format!("pulse reader: {e}"))?;
    let beats = pulse
        .all_beats()
        .await
        .map_err(|e| format!("read fleet pulse: {e}"))?;
    let reserve_gb = std::env::var("FORGEFLEET_BUILD_RESERVE_GB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_BUILD_RESERVE_GB);
    let node = ff_db::pg_get_node(pool, worker_name)
        .await
        .map_err(|e| format!("get node {worker_name}: {e}"))?
        .ok_or_else(|| format!("worker {worker_name} is not registered"))?;

    let mut completed = 0;
    for candidate in candidates {
        if candidate.gated || !permissive_license(candidate.license.as_deref()) {
            tracing::warn!(
                catalog_id = %candidate.id,
                license = ?candidate.license,
                "watchlist: refusing gated or non-permissively-licensed model"
            );
            continue;
        }
        let Some(variant) = choose_variant(&candidate.variants) else {
            tracing::warn!(catalog_id = %candidate.id, "watchlist: no verified downloadable variant");
            continue;
        };
        let required_gb = variant.size_gb + reserve_gb;
        if choose_node(&beats, required_gb) != Some(worker_name) {
            continue;
        }

        let mut connection = pool
            .acquire()
            .await
            .map_err(|e| format!("acquire watchlist lock connection: {e}"))?;
        let lock_key = format!("model-watchlist:{}:{}", candidate.id, variant.quant);
        let locked: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
                .bind(&lock_key)
                .fetch_one(&mut *connection)
                .await
                .map_err(|e| format!("acquire watchlist lock: {e}"))?;
        if !locked {
            continue;
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM fleet_model_library WHERE catalog_id = $1)",
        )
        .bind(&candidate.id)
        .fetch_one(&mut *connection)
        .await
        .map_err(|e| format!("recheck library: {e}"))?;
        if exists {
            continue;
        }

        let dest_dir = expand_models_dir(&node.models_dir).join(&candidate.id);
        let repo = variant.hf_repo.clone();
        let progress_id = candidate.id.clone();
        let files = crate::hf_download::download_repo(
            &reqwest::Client::new(),
            crate::hf_download::DownloadOptions {
                repo: repo.clone(),
                dest_dir: dest_dir.clone(),
                allow_patterns: variant.allow_patterns.clone(),
                deny_patterns: vec!["*.safetensors".into(), "*.bin".into()],
                ..Default::default()
            },
            move |progress| {
                tracing::info!(
                    catalog_id = %progress_id,
                    file = %progress.file,
                    percent = progress.percent,
                    "watchlist download progress"
                );
            },
        )
        .await?;
        let size_bytes = directory_size(&dest_dir) as i64;
        if files.is_empty() || size_bytes == 0 {
            return Err(format!(
                "watchlist download {} produced no files",
                candidate.id
            ));
        }
        let library_id = ff_db::pg_upsert_library(
            pool,
            worker_name,
            &candidate.id,
            &variant.runtime,
            Some(&variant.quant),
            &dest_dir.to_string_lossy(),
            size_bytes,
            None,
            Some(&format!("https://huggingface.co/{repo}")),
        )
        .await
        .map_err(|e| format!("register downloaded model {}: {e}", candidate.id))?;
        sqlx::query("UPDATE fleet_model_library SET state = 'cold' WHERE id = $1::uuid")
            .bind(&library_id)
            .execute(pool)
            .await
            .map_err(|e| format!("mark downloaded model {} cold: {e}", candidate.id))?;
        completed += 1;
        tracing::info!(
            catalog_id = %candidate.id,
            worker = worker_name,
            size_bytes,
            "watchlist model registered cold"
        );
    }
    Ok(completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn license_policy_only_accepts_known_permissive_licenses() {
        assert!(permissive_license(Some("MIT")));
        assert!(permissive_license(Some("apache-2.0")));
        assert!(!permissive_license(None));
        assert!(!permissive_license(Some("gemma")));
    }

    #[test]
    fn chooses_smallest_valid_llamacpp_variant() {
        let variants = serde_json::json!([
            {"runtime":"mlx","quant":"4bit","hf_repo":"x/mlx","size_gb":4.0},
            {"runtime":"llama.cpp","quant":"Q5","hf_repo":"x/q5","size_gb":10.0},
            {"runtime":"llama.cpp","quant":"Q4","hf_repo":"x/q4","size_gb":8.0}
        ]);
        assert_eq!(choose_variant(&variants).unwrap().quant, "Q4");
    }

    #[test]
    fn reserve_boundary_requires_both_disk_and_ram() {
        let mut beat = PulseBeatV2::skeleton("node");
        beat.load.disk_free_gb = 15.0;
        beat.memory.ram_available_for_new_llm_gb = 14.99;
        assert!(!node_fits(&beat, 15.0));
        beat.memory.ram_available_for_new_llm_gb = 15.0;
        assert!(node_fits(&beat, 15.0));
    }

    #[test]
    fn placement_prefers_combined_headroom_then_stable_name() {
        let mut alpha = PulseBeatV2::skeleton("alpha");
        alpha.load.disk_free_gb = 30.0;
        alpha.memory.ram_available_for_new_llm_gb = 20.0;
        let mut beta = PulseBeatV2::skeleton("beta");
        beta.load.disk_free_gb = 20.0;
        beta.memory.ram_available_for_new_llm_gb = 30.0;
        assert_eq!(choose_node(&[beta, alpha], 10.0), Some("alpha"));
    }
}
