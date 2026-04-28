//! Structured parsing of `ff --version` / `forgefleetd --version` output.
//!
//! The version string format is `<date>_<n> (<state> <sha>)`, e.g.
//! `2026.4.27_64 (pushed db1a950e4c)`. The leading `<date>_<n>` is a
//! per-machine same-day **build counter** — identical hosts at the
//! same git SHA will report different counters depending on how often
//! that specific machine has rebuilt today. The `<sha>` is the
//! **code identity**: same SHA = same code, regardless of counter.
//!
//! Display call sites that lead with the build counter (e.g. an
//! ad-hoc verify table that shows "ace=2026.4.27_24, marcus=2026.4.27_12")
//! mislead the reader into thinking the hosts are at different versions
//! when in fact they're running the same commit. Use [`BuildVersion::parse`]
//! and [`BuildVersion::short_sha`] / [`code_identity`] when you want to
//! show "is this the same code" — never the raw counter.

/// Parsed shape of a `ff --version` / `forgefleetd --version` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildVersion {
    /// `YYYY.M.D` build date as emitted by `build_version.rs`.
    pub date: String,
    /// Per-machine same-day build counter. Identical hosts at the same
    /// SHA legitimately differ here.
    pub build_count: u32,
    /// `pushed` | `unpushed` | `dirty` | `unknown`.
    pub state: String,
    /// The 10-char git SHA prefix — the **only field that means "same code"**.
    pub sha: String,
}

impl BuildVersion {
    /// Parse `<date>_<n> (<state> <sha>)`, optionally prefixed by a
    /// binary name (e.g. `ff 2026.4.27_64 (pushed db1a950e4c)`).
    /// Returns `None` if the shape doesn't match — caller decides
    /// whether to fall back to a raw display.
    pub fn parse(input: &str) -> Option<Self> {
        let s = input.trim();
        // Strip an optional binary-name prefix like "ff " or "forgefleet ".
        // Heuristic: if the first space-delimited token is alphabetic (a
        // binary name) rather than digits-and-dots (a date), strip it.
        let s = match s.split_once(' ') {
            Some((first, rest)) if !first.chars().any(|c| c.is_ascii_digit()) => rest,
            _ => s,
        };
        let (date_count, paren) = s.split_once(" (")?;
        let (date, count) = date_count.split_once('_')?;
        let build_count: u32 = count.parse().ok()?;
        let inner = paren.strip_suffix(')')?;
        let (state, sha) = inner.split_once(' ')?;
        Some(BuildVersion {
            date: date.to_string(),
            build_count,
            state: state.to_string(),
            sha: sha.to_string(),
        })
    }

    /// 8-char prefix of the SHA, suitable for narrow display columns.
    /// Same code → same value across all hosts.
    pub fn short_sha(&self) -> &str {
        let n = self.sha.chars().count().min(8);
        &self.sha[..n]
    }

    /// True iff `other` reports the same git SHA. Build count, date,
    /// and state are explicitly ignored — they're per-host activity,
    /// not code identity.
    pub fn is_same_code(&self, other: &Self) -> bool {
        self.sha == other.sha
    }
}

/// Convenience: extract the code-identity SHA prefix from a raw
/// `--version` string. Returns the input unchanged when parsing fails
/// (e.g. an `unknown` build or a non-ff binary), which keeps display
/// callers usable even on malformed input — but they should ideally
/// log the parse failure and fall back to the raw string explicitly.
pub fn code_identity(version_string: &str) -> String {
    BuildVersion::parse(version_string)
        .map(|v| v.short_sha().to_string())
        .unwrap_or_else(|| version_string.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pushed_form() {
        let v = BuildVersion::parse("ff 2026.4.27_64 (pushed db1a950e4c)").unwrap();
        assert_eq!(v.date, "2026.4.27");
        assert_eq!(v.build_count, 64);
        assert_eq!(v.state, "pushed");
        assert_eq!(v.sha, "db1a950e4c");
        assert_eq!(v.short_sha(), "db1a950e");
    }

    #[test]
    fn parses_without_binary_prefix() {
        let v = BuildVersion::parse("2026.4.27_12 (pushed db1a950e4c)").unwrap();
        assert_eq!(v.build_count, 12);
        assert_eq!(v.sha, "db1a950e4c");
    }

    #[test]
    fn parses_dirty_state() {
        let v = BuildVersion::parse("forgefleet 2026.4.27_3 (dirty 8355028d12)").unwrap();
        assert_eq!(v.state, "dirty");
        assert_eq!(v.sha, "8355028d12");
    }

    #[test]
    fn same_code_ignores_build_count_and_date() {
        let a = BuildVersion::parse("ff 2026.4.27_12 (pushed db1a950e4c)").unwrap();
        let b = BuildVersion::parse("ff 2026.4.27_64 (pushed db1a950e4c)").unwrap();
        let c = BuildVersion::parse("ff 2026.4.28_1 (pushed db1a950e4c)").unwrap();
        assert!(a.is_same_code(&b));
        assert!(a.is_same_code(&c));
    }

    #[test]
    fn different_sha_is_not_same_code() {
        let a = BuildVersion::parse("ff 2026.4.27_64 (pushed db1a950e4c)").unwrap();
        let b = BuildVersion::parse("ff 2026.4.27_64 (pushed 33e05f9beb)").unwrap();
        assert!(!a.is_same_code(&b));
    }

    #[test]
    fn malformed_returns_none() {
        assert!(BuildVersion::parse("ff 2026.4.27").is_none());
        assert!(BuildVersion::parse("garbage").is_none());
        assert!(BuildVersion::parse("ff unknown (unknown unknown)").is_none());
    }

    #[test]
    fn code_identity_of_well_formed() {
        let s = code_identity("ff 2026.4.27_64 (pushed db1a950e4c)");
        assert_eq!(s, "db1a950e");
    }

    #[test]
    fn code_identity_falls_back_on_garbage() {
        let s = code_identity("garbage in");
        assert_eq!(s, "garbage in");
    }
}
