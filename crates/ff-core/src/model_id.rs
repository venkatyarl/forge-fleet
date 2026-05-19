//! Canonical model-identifier normalization.
//!
//! LLM-server pulse beats report `model.id` in many shapes — filesystem
//! paths (`/Users/venkat/models/qwen36-35b-a3b`), GGUF filenames with
//! quant + case (`Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf`), HF repo
//! strings (`mlx-community/qwen3-coder-30b-a3b-4bit`), Ollama tags
//! (`qwen3-coder:30b`), and the bare catalog id. Routing decisions
//! across the fleet need every comparison to go through one canonical
//! form, otherwise pool aliases / model matching silently drop valid
//! candidates.
//!
//! Shared by `ff-gateway::llm_routing` (request-time routing) and
//! `ff-pulse::reader` (pool-alias expansion). Before this consolidation
//! the two crates kept their own copies which drifted on the second
//! pool-alias fix (FA.2).

/// Normalize a model identifier to a comparable form. Lowercase, strip
/// path/HF-repo prefix, drop Ollama tag suffix, strip file extension,
/// fold separators, strip quant/precision suffix. Idempotent.
pub fn normalize_model_id(raw: &str) -> String {
    let mut s = raw.to_ascii_lowercase();

    if let Some(idx) = s.rfind('/') {
        s = s[idx + 1..].to_string();
    }
    if let Some(idx) = s.find(':') {
        s.truncate(idx);
    }
    for ext in [".gguf", ".bin", ".safetensors"] {
        if s.ends_with(ext) {
            s.truncate(s.len() - ext.len());
            break;
        }
    }
    s = s.replace('_', "-");
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let quant_suffixes: &[&str] = &[
        "-q2-k", "-q3-k-s", "-q3-k-m", "-q3-k-l", "-q4-0", "-q4-1", "-q4-k-s", "-q4-k-m", "-q5-0",
        "-q5-1", "-q5-k-s", "-q5-k-m", "-q6-k", "-q8-0", "-bf16", "-fp16", "-fp8", "-f16", "-f32",
        "-int8", "-int4", "-awq", "-gptq",
    ];
    loop {
        let mut changed = false;
        for sfx in quant_suffixes {
            if s.ends_with(sfx) {
                s.truncate(s.len() - sfx.len());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // Insert a dash at every letter→digit boundary so `gemma4-31b-it` and
    // `gemma-4-31b-it` collapse to the same canonical form. Catalog ids
    // using the compact form (`gemma4`) used to fail prefix-match against
    // pulse beats that use the dashed form (`gemma-4-31B-it.gguf` from
    // unsloth). Applied symmetrically to both sides of every comparison,
    // so internal consistency holds. (CAT.1, 2026-05-19.)
    //
    // Runs AFTER quant-suffix stripping so the suffix list (e.g. `-q4-k-m`)
    // still matches GGUF filenames that originally read `Q4_K_M`.
    // Only letter→digit (not digit→letter) — leaving `31b` intact.
    let mut out = String::with_capacity(s.len() + 2);
    let mut prev_is_letter = false;
    for ch in s.chars() {
        let is_digit = ch.is_ascii_digit();
        if prev_is_letter && is_digit {
            out.push('-');
        }
        out.push(ch);
        prev_is_letter = ch.is_ascii_alphabetic();
    }
    s = out;
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_path_to_catalog_id() {
        // After CAT.1 letter→digit dash insertion, both compact and dashed
        // catalog ids land on the same canonical form.
        assert_eq!(
            normalize_model_id("/Users/venkat/models/qwen36-35b-a3b"),
            "qwen-36-35b-a-3b"
        );
        assert_eq!(
            normalize_model_id("qwen36-35b-a3b"),
            normalize_model_id("/Users/venkat/models/qwen36-35b-a3b")
        );
    }

    #[test]
    fn gguf_filename_drops_extension_and_quant() {
        // Note: after CAT.1 the letter→digit boundary becomes dashed
        // (`qwen-3-coder-30b-a-3b-instruct`), but the round-trip and
        // symmetric comparison still hold.
        assert_eq!(
            normalize_model_id("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf"),
            "qwen-3-coder-30b-a-3b-instruct"
        );
    }

    #[test]
    fn ollama_tag_dropped() {
        // Ollama uses `model:tag` form; everything after `:` is stripped.
        // CAT.1 also applies letter→digit insertion to the model portion.
        assert_eq!(normalize_model_id("qwen3-coder:30b"), "qwen-3-coder");
        assert_eq!(normalize_model_id("gemma2:9b"), "gemma-2");
    }

    #[test]
    fn hf_repo_strips_org() {
        assert_eq!(
            normalize_model_id("mlx-community/qwen3-coder-30b-a3b-4bit"),
            "qwen-3-coder-30b-a-3b-4bit"
        );
    }

    #[test]
    fn idempotent() {
        let once = normalize_model_id("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf");
        let twice = normalize_model_id(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn empty_input_safe() {
        assert_eq!(normalize_model_id(""), "");
    }

    #[test]
    fn gemma_dash_drift_collapses() {
        // CAT.1: catalog id and GGUF filename must compare equal after
        // normalization, even though one uses `gemma4` (compact) and the
        // other `gemma-4-31B-it.gguf` (dashed). Both should land on
        // `gemma-4-31b-it`.
        let catalog = normalize_model_id("gemma4-31b-it");
        let beat = normalize_model_id("gemma-4-31B-it-Q4_K_M.gguf");
        assert_eq!(catalog, beat);
        assert_eq!(catalog, "gemma-4-31b-it");
    }

    #[test]
    fn qwen_letter_digit_inserts_dash() {
        assert_eq!(normalize_model_id("qwen3-coder-30b"), "qwen-3-coder-30b");
        assert_eq!(
            normalize_model_id("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf"),
            "qwen-3-coder-30b-a-3b-instruct"
        );
        // Both sides collapse so the gateway's prefix match still hits.
        let req = normalize_model_id("qwen3-coder-30b");
        let beat = normalize_model_id("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf");
        assert!(beat.starts_with(&req));
    }
}
