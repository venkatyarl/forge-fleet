//! Canonical model-identifier normalization.
//!
//! LLM-server pulse beats report `model.id` in many shapes — filesystem
//! paths (`/Users/venkat/models/qwen36-35b-a3b`), GGUF filenames with
//! quant + case (`Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf`), HF repo
//! strings (`mlx-community/qwen3-coder-30b-a3b-4bit`), Ollama tags
//! (`qwen2.5-coder:14b`), and the bare catalog id. Routing decisions
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
    s.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_path_to_catalog_id() {
        assert_eq!(
            normalize_model_id("/Users/venkat/models/qwen36-35b-a3b"),
            "qwen36-35b-a3b"
        );
    }

    #[test]
    fn gguf_filename_drops_extension_and_quant() {
        assert_eq!(
            normalize_model_id("Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf"),
            "qwen3-coder-30b-a3b-instruct"
        );
    }

    #[test]
    fn ollama_tag_dropped() {
        assert_eq!(normalize_model_id("qwen2.5-coder:14b"), "qwen2.5-coder");
    }

    #[test]
    fn hf_repo_strips_org() {
        assert_eq!(
            normalize_model_id("mlx-community/qwen3-coder-30b-a3b-4bit"),
            "qwen3-coder-30b-a3b-4bit"
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
}
