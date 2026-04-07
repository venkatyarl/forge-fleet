//! Feature flag system — runtime feature toggles.
//!
//! Flags are loaded from fleet.toml [features] section or environment variables.
//! Lightweight HashMap-based flags with env var overrides.

use std::collections::HashMap;
use std::sync::RwLock;

/// Global feature flag store.
static FLAGS: std::sync::LazyLock<RwLock<HashMap<String, bool>>> =
    std::sync::LazyLock::new(|| RwLock::new(default_flags()));

fn default_flags() -> HashMap<String, bool> {
    let mut flags = HashMap::new();

    // Core features
    flags.insert("agent_tools".into(), true);
    flags.insert("auto_compaction".into(), true);
    flags.insert("session_persistence".into(), true);
    flags.insert("tool_result_budgeting".into(), true);
    flags.insert("token_tracking".into(), true);

    // Agent features
    flags.insert("sub_agents".into(), true);
    flags.insert("web_tools".into(), true);
    flags.insert("task_tools".into(), true);
    flags.insert("plan_mode".into(), true);
    flags.insert("worktree_tools".into(), true);

    // Security
    flags.insert("permission_checks".into(), false); // off by default for dev
    flags.insert("secret_detection".into(), true);
    flags.insert("blocked_paths".into(), true);
    flags.insert("bash_classifier".into(), true);

    // Advanced (off by default)
    flags.insert("hooks".into(), false);
    flags.insert("memory_extraction".into(), false);
    flags.insert("voice".into(), false);
    flags.insert("mcp_client".into(), false);

    flags
}

/// Check if a feature is enabled.
pub fn is_enabled(name: &str) -> bool {
    // Check environment variable first (FORGEFLEET_FEATURE_xxx=true)
    let env_key = format!("FORGEFLEET_FEATURE_{}", name.to_uppercase());
    if let Ok(val) = std::env::var(&env_key) {
        return val == "1" || val.eq_ignore_ascii_case("true");
    }

    FLAGS
        .read()
        .ok()
        .and_then(|flags| flags.get(name).copied())
        .unwrap_or(false)
}

/// Set a feature flag at runtime.
pub fn set(name: &str, enabled: bool) {
    if let Ok(mut flags) = FLAGS.write() {
        flags.insert(name.to_string(), enabled);
    }
}

/// Get all feature flags and their current values.
pub fn all() -> Vec<(String, bool)> {
    FLAGS
        .read()
        .map(|flags| {
            let mut entries: Vec<_> = flags.iter().map(|(k, v)| (k.clone(), *v)).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
        })
        .unwrap_or_default()
}

/// Load feature flags from a TOML-style key=value map.
pub fn load_from_map(map: &HashMap<String, bool>) {
    if let Ok(mut flags) = FLAGS.write() {
        for (k, v) in map {
            flags.insert(k.clone(), *v);
        }
    }
}
