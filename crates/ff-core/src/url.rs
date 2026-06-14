//! Canonical OpenAI-compatible endpoint URL normalization.
//!
//! Fleet code builds the chat-completions URL from many endpoint shapes:
//! a bare host:port base (`http://192.168.5.102:55000`), a `/v1` base, or
//! — when an operator passes `--llm` or a DB-stored endpoint already carries
//! the full path — a complete `http://host:port/v1/chat/completions`.
//!
//! Before this consolidation every call site did
//! `format!("{}/v1/chat/completions", base.trim_end_matches('/'))`, which
//! silently DOUBLES the path when `base` already ends in
//! `/v1/chat/completions` → `…/v1/chat/completions/v1/chat/completions` →
//! a 404 on every turn (P1.6 in the improvement plan; `ff run --llm
//! http://host:port/v1/chat/completions` hung with silent 404s). Routing
//! every site through one helper makes the append idempotent.

/// Normalize any OpenAI-compatible endpoint base into the full
/// chat-completions URL. Accepts a bare base, a `/v1` base, or an already
/// complete `/v1/chat/completions` URL. Trailing slashes are tolerated.
/// Idempotent: applying it to its own output is a no-op.
pub fn normalize_chat_completions_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1/chat/completions") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "http://host:55000/v1/chat/completions";

    #[test]
    fn bare_base_gets_full_path() {
        assert_eq!(normalize_chat_completions_url("http://host:55000"), FULL);
    }

    #[test]
    fn trailing_slash_tolerated() {
        assert_eq!(normalize_chat_completions_url("http://host:55000/"), FULL);
    }

    #[test]
    fn v1_base_gets_chat_completions() {
        assert_eq!(normalize_chat_completions_url("http://host:55000/v1"), FULL);
        assert_eq!(
            normalize_chat_completions_url("http://host:55000/v1/"),
            FULL
        );
    }

    #[test]
    fn full_url_is_not_doubled() {
        // The P1.6 bug: this used to become
        // …/v1/chat/completions/v1/chat/completions
        assert_eq!(normalize_chat_completions_url(FULL), FULL);
        assert_eq!(normalize_chat_completions_url(&format!("{FULL}/")), FULL);
    }

    #[test]
    fn idempotent() {
        let once = normalize_chat_completions_url("http://host:55000");
        let twice = normalize_chat_completions_url(&once);
        assert_eq!(once, twice);
    }
}
