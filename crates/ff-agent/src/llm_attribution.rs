//! Engine / token / cost attribution for `ff_interactions` logging.
//!
//! Usage audit 2026-07-20: 24h of `ff_interactions` showed `engine` empty for
//! local-model calls, `tokens_in` almost always 0 and `cost_usd` always 0 —
//! every logging call site invented its own half of the attribution, so the
//! LLM-usage rollups undercounted. This module is the one shared vocabulary all
//! call sites (dispatch, council, mcp, offload, research, gateway, …) use:
//!
//! - [`engine_label`] — canonical `engine` value: a vendor CLI name passes
//!   through unchanged (`claude`/`codex`/`kimi`/…); anything else is a local
//!   model and becomes `local:<catalog_id>` (same id derivation the deployment
//!   reconciler uses, so quants of one model collapse to one engine).
//! - [`parse_cli_token_counts`] — scrape prompt/completion counts from a cloud
//!   CLI's stdout+stderr (JSON usage keys, else the codex "tokens used" line).
//! - [`tokens_or_estimate`] — chars/4 fallback so a call whose token counts
//!   aren't reported still lands in the usage rollup as a flagged estimate
//!   instead of 0.
//! - [`cost_usd`] — per-token USD cost from the operator-editable rates table
//!   (`engine_rates.toml`, NOT hardcoded); `local:*` engines are $0.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::cli_executor::BACKENDS;
use crate::deployment_catalog_reconciler::derive_catalog_id;

// ─── Engine label ────────────────────────────────────────────────────────────

/// Canonical `ff_interactions.engine` value for whatever served a call.
///
/// Vendor CLI backends keep their canonical lowercase name; everything else is
/// a local model name/filename and becomes `local:<catalog_id>`. Never returns
/// an empty string — an unnameable local model degrades to plain `local`.
pub fn engine_label(model: &str) -> String {
    let m = model.trim();
    if m.is_empty() || m.eq_ignore_ascii_case("local") {
        return "local".to_string();
    }
    if let Some(b) = BACKENDS.iter().find(|b| b.name.eq_ignore_ascii_case(m)) {
        return b.name.to_string();
    }
    // Already-prefixed (`local:<hint>`) values re-derive so a quant-suffixed
    // hint still collapses to the catalog id.
    let raw = m.strip_prefix("local:").unwrap_or(m);
    let id = derive_catalog_id(raw);
    if id.is_empty() {
        "local".to_string()
    } else {
        format!("local:{id}")
    }
}

// ─── Token parsing / estimation ──────────────────────────────────────────────

/// Parse `(tokens_in, tokens_out)` from a cloud CLI's combined stdout+stderr.
///
/// Looks for OpenAI/Anthropic-shape JSON usage keys anywhere in the text
/// (`"input_tokens"`/`"prompt_tokens"` and `"output_tokens"`/
/// `"completion_tokens"`), taking the LAST occurrence so streamed JSONL with
/// cumulative counts yields the final total. When no JSON usage is present it
/// falls back to the total-only text marker (`tokens used: N` — codex), which
/// lands in the `tokens_out` slot (the historical `parse_cli_tokens`
/// convention). `(0, 0)` when nothing is found. Pure.
pub fn parse_cli_token_counts(output: &str) -> (i32, i32) {
    let tin = scan_last_uint_after(output, &["\"input_tokens\"", "\"prompt_tokens\""]);
    let tout = scan_last_uint_after(output, &["\"output_tokens\"", "\"completion_tokens\""]);
    if tin.is_some() || tout.is_some() {
        let clamp = |n: Option<i64>| i32::try_from(n.unwrap_or(0)).unwrap_or(i32::MAX);
        return (clamp(tin), clamp(tout));
    }
    (0, parse_total_tokens_marker(output))
}

/// Total-only token scrape from human-facing CLI output (`tokens used
/// 9,332`, `Total tokens: 1234`, …). Case-insensitive; first marker wins.
/// This is the legacy `work_item_dispatch::parse_cli_tokens` behavior, moved
/// here so every wrapper shares it. Pure.
pub fn parse_total_tokens_marker(output: &str) -> i32 {
    let lower = output.to_lowercase();
    for marker in ["tokens used", "total tokens", "tokens:", "tokens"] {
        if let Some(pos) = lower.find(marker) {
            let tail = &output[pos + marker.len()..];
            let digits: String = tail
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .filter(|c| *c != ',')
                .collect();
            if let Ok(n) = digits.parse::<i32>() {
                return n;
            }
        }
    }
    0
}

/// Last unsigned integer following any of `patterns` (JSON-key style: skips
/// `:`, quotes and whitespace, then reads digits with optional `,` grouping).
/// The first pattern with any match wins; within it the last occurrence is
/// returned (streamed cumulative counts end with the total).
fn scan_last_uint_after(output: &str, patterns: &[&str]) -> Option<i64> {
    for pat in patterns {
        let mut found = None;
        let mut start = 0;
        while let Some(pos) = output[start..].find(pat) {
            let abs = start + pos + pat.len();
            let digits: String = output[abs..]
                .chars()
                .skip_while(|c| c.is_whitespace() || *c == ':' || *c == '"')
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .filter(|c| *c != ',')
                .collect();
            if let Ok(n) = digits.parse::<i64>() {
                found = Some(n);
            }
            start = abs;
        }
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Rough token estimate for text with no reported usage: ~4 chars/token,
/// at least 1 for non-empty text. Pure.
pub fn estimate_tokens(text: &str) -> i32 {
    if text.trim().is_empty() {
        return 0;
    }
    i32::try_from(text.chars().count().div_ceil(4)).unwrap_or(i32::MAX)
}

/// Pass real token counts through; fill any zero side with a chars/4 estimate
/// from the request/response text. Returns `(tokens_in, tokens_out, estimated)`
/// where `estimated` is true when any side was estimated — callers flag it in
/// `request_meta` so rollups can tell measured from estimated usage. Pure.
pub fn tokens_or_estimate(
    tokens_in: i32,
    tokens_out: i32,
    request: &str,
    response: &str,
) -> (i32, i32, bool) {
    let mut estimated = false;
    let mut fill = |reported: i32, text: &str| {
        if reported > 0 {
            return reported;
        }
        let est = estimate_tokens(text);
        if est > 0 {
            estimated = true;
        }
        est
    };
    let tin = fill(tokens_in, request);
    let tout = fill(tokens_out, response);
    (tin, tout, estimated)
}

// ─── Cost rates (config-driven, never hardcoded) ─────────────────────────────

/// Published per-token USD rate for one cloud engine, in USD per 1M tokens.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct EngineRate {
    #[serde(default)]
    pub input_per_mtok: f64,
    #[serde(default)]
    pub output_per_mtok: f64,
}

/// Engine name (lowercase) → published rate.
pub type EngineRates = HashMap<String, EngineRate>;

/// Fallback rates path in the operator's main checkout (same convention as
/// `model_catalog::DEFAULT_CATALOG_PATH`).
const DEFAULT_RATES_REPO_PATH: &str = "/Users/venkat/projects/forge-fleet/config/engine_rates.toml";

/// Search order for the rates TOML: env override, per-node home config, then
/// the repo copy (cwd-relative for in-repo runs, absolute operator checkout).
fn rates_path_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("FORGEFLEET_ENGINE_RATES") {
        v.push(PathBuf::from(p));
    }
    if let Ok(h) = std::env::var("HOME") {
        v.push(
            PathBuf::from(h)
                .join(".forgefleet")
                .join("engine_rates.toml"),
        );
    }
    v.push(PathBuf::from("config/engine_rates.toml"));
    v.push(PathBuf::from(DEFAULT_RATES_REPO_PATH));
    v
}

/// Load the first parseable rates file, keys lowercased. Missing/broken files
/// degrade to an empty table (cost 0) — attribution must never fail a call.
pub fn load_engine_rates() -> EngineRates {
    for path in rates_path_candidates() {
        if !path.is_file() {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|raw| toml::from_str::<EngineRates>(&raw).map_err(|e| e.to_string()));
        match parsed {
            Ok(map) => {
                return map
                    .into_iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), v))
                    .collect();
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "engine_rates: unreadable rates file — trying next candidate");
            }
        }
    }
    EngineRates::new()
}

fn cached_rates() -> &'static EngineRates {
    static RATES: OnceLock<EngineRates> = OnceLock::new();
    RATES.get_or_init(load_engine_rates)
}

/// USD cost of one call. `local`/`local:*` engines are $0 by definition; cloud
/// engines use the published rates table, degrading to $0 when unlisted.
pub fn cost_usd(engine: &str, tokens_in: i32, tokens_out: i32) -> f64 {
    cost_usd_from(cached_rates(), engine, tokens_in, tokens_out)
}

/// Pure core of [`cost_usd`] — unit-testable with a hand-built table.
pub fn cost_usd_from(rates: &EngineRates, engine: &str, tokens_in: i32, tokens_out: i32) -> f64 {
    let key = engine.trim().to_ascii_lowercase();
    if key == "local" || key.starts_with("local:") {
        return 0.0;
    }
    let Some(rate) = rates.get(&key) else {
        return 0.0;
    };
    (f64::from(tokens_in.max(0)) * rate.input_per_mtok
        + f64::from(tokens_out.max(0)) * rate.output_per_mtok)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_label_passes_vendor_names_through() {
        assert_eq!(engine_label("claude"), "claude");
        assert_eq!(engine_label("Codex"), "codex");
        assert_eq!(engine_label("KIMI"), "kimi");
    }

    #[test]
    fn engine_label_prefixes_local_models_with_catalog_id() {
        assert_eq!(
            engine_label("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf"),
            "local:qwen3-coder-30b-a3b-instruct"
        );
        assert_eq!(engine_label("qwen36-35b"), "local:qwen36-35b");
        // Already-prefixed hints normalize instead of double-prefixing.
        assert_eq!(engine_label("local:qwen36-35b"), "local:qwen36-35b");
    }

    #[test]
    fn engine_label_never_empty() {
        assert_eq!(engine_label(""), "local");
        assert_eq!(engine_label("  "), "local");
        assert_eq!(engine_label("local"), "local");
    }

    #[test]
    fn parses_json_usage_keys() {
        let claude = r#"{"type":"result","usage":{"input_tokens":100,"output_tokens":25}}"#;
        assert_eq!(parse_cli_token_counts(claude), (100, 25));
        let openai = r#"{"usage":{"prompt_tokens":17,"completion_tokens":5}}"#;
        assert_eq!(parse_cli_token_counts(openai), (17, 5));
    }

    #[test]
    fn parses_last_streamed_usage() {
        // Streamed JSONL with cumulative counts: the last event is the total.
        let jsonl = "{\"input_tokens\": 10, \"output_tokens\": 2}\n{\"input_tokens\": 50, \"output_tokens\": 9}";
        assert_eq!(parse_cli_token_counts(jsonl), (50, 9));
    }

    #[test]
    fn json_keys_do_not_match_cached_variants() {
        // "cached_input_tokens" must not satisfy the "input_tokens" scan — the
        // quoted-key pattern only matches the standalone key.
        let mixed = r#"{"cached_input_tokens": 999, "input_tokens": 10, "output_tokens": 2}"#;
        assert_eq!(parse_cli_token_counts(mixed), (10, 2));
    }

    #[test]
    fn falls_back_to_total_marker() {
        assert_eq!(
            parse_cli_token_counts("codex\nOK\ntokens used\n9,332\n"),
            (0, 9332)
        );
        assert_eq!(parse_cli_token_counts("Total tokens: 1234"), (0, 1234));
        assert_eq!(parse_cli_token_counts("no counts here"), (0, 0));
    }

    #[test]
    fn estimates_fill_missing_counts_and_flag() {
        let req = "x".repeat(400);
        let (tin, tout, est) = tokens_or_estimate(0, 0, &req, "eight ch");
        assert_eq!((tin, tout), (100, 2));
        assert!(est);
        // Real counts pass through untouched.
        assert_eq!(tokens_or_estimate(17, 5, &req, "resp"), (17, 5, false));
        // Nothing to estimate from → still 0, not flagged.
        assert_eq!(tokens_or_estimate(0, 0, "", ""), (0, 0, false));
    }

    #[test]
    fn cost_uses_rates_table_and_zeroes_local() {
        let mut rates = EngineRates::new();
        rates.insert(
            "codex".into(),
            EngineRate {
                input_per_mtok: 1.25,
                output_per_mtok: 10.0,
            },
        );
        let c = cost_usd_from(&rates, "codex", 1_000_000, 1_000_000);
        assert!((c - 11.25).abs() < 1e-9);
        assert_eq!(
            cost_usd_from(&rates, "local:qwen3-coder-30b", 1_000_000, 1_000_000),
            0.0
        );
        assert_eq!(cost_usd_from(&rates, "local", 5, 5), 0.0);
        // Unlisted cloud engine degrades to 0, never panics.
        assert_eq!(cost_usd_from(&rates, "claude", 100, 100), 0.0);
    }

    /// The rates file shipped in-repo must parse and cover the vendor CLIs the
    /// dispatch actually uses — a broken TOML would silently zero all costs.
    #[test]
    fn shipped_rates_file_parses() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/engine_rates.toml"
        );
        let raw = std::fs::read_to_string(path).expect("read config/engine_rates.toml");
        let rates: EngineRates = toml::from_str(&raw).expect("parse config/engine_rates.toml");
        for engine in ["claude", "codex", "kimi"] {
            let r = rates.get(engine).expect("vendor rate present");
            assert!(r.input_per_mtok > 0.0 && r.output_per_mtok > 0.0);
        }
    }
}
