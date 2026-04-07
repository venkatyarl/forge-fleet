//! Permission system — per-tool permission levels and enforcement.
//!
//! Controls which tools can execute automatically vs. requiring user approval.
//! ForgeFleet's 4-mode permission model for safe autonomous operation.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Permission mode for the agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Default: read-only tools auto-allowed, write/execute tools ask.
    Default,
    /// Accept all edits automatically (no prompts for file changes).
    AcceptEdits,
    /// Bypass all permission checks (dangerous).
    Bypass,
    /// Plan mode: only read-only tools allowed.
    Plan,
}

impl Default for PermissionMode {
    fn default() -> Self { Self::Default }
}

/// Permission level for a tool operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    /// No permission needed (e.g., asking user a question).
    None,
    /// Read-only filesystem access.
    ReadOnly,
    /// Write to filesystem.
    Write,
    /// Execute shell commands.
    Execute,
    /// Dangerous operations (destructive, network-facing).
    Dangerous,
    /// Unconditionally blocked.
    Forbidden,
}

/// Decision for a permission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Allow the operation.
    Allow,
    /// Deny the operation.
    Deny,
    /// Ask the user for approval (interactive mode only).
    Ask,
}

/// Blocked file patterns that should never be read or written.
const BLOCKED_PATHS: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    "credentials.json",
    "service-account.json",
    ".aws/credentials",
    ".ssh/id_rsa",
    ".ssh/id_ed25519",
    "*.pem",
    "*.key",
    "secrets.yaml",
    "secrets.yml",
    ".npmrc",  // may contain tokens
    ".pypirc", // may contain tokens
];

/// Check if a file path should be blocked.
pub fn is_blocked_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    BLOCKED_PATHS.iter().any(|pattern| {
        if pattern.starts_with('*') {
            lower.ends_with(&pattern[1..])
        } else {
            lower.ends_with(pattern) || lower.contains(&format!("/{pattern}"))
        }
    })
}

/// Permission configuration for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionConfig {
    pub mode: PermissionMode,
    /// Tools that are always allowed regardless of mode.
    pub allowlist: HashSet<String>,
    /// Tools that are always denied.
    pub denylist: HashSet<String>,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Default,
            allowlist: HashSet::new(),
            denylist: HashSet::new(),
        }
    }
}

/// Check if a tool operation should be allowed.
pub fn check_permission(
    tool_name: &str,
    level: PermissionLevel,
    config: &PermissionConfig,
) -> PermissionDecision {
    // Explicit denylist
    if config.denylist.contains(tool_name) {
        return PermissionDecision::Deny;
    }

    // Explicit allowlist
    if config.allowlist.contains(tool_name) {
        return PermissionDecision::Allow;
    }

    // Forbidden is always denied
    if level == PermissionLevel::Forbidden {
        return PermissionDecision::Deny;
    }

    match config.mode {
        PermissionMode::Bypass => PermissionDecision::Allow,
        PermissionMode::Plan => {
            // Plan mode: only read-only and below
            if level <= PermissionLevel::ReadOnly {
                PermissionDecision::Allow
            } else {
                PermissionDecision::Deny
            }
        }
        PermissionMode::AcceptEdits => {
            // Accept edits: allow up to Write, ask for Execute+
            if level <= PermissionLevel::Write {
                PermissionDecision::Allow
            } else if level == PermissionLevel::Execute {
                PermissionDecision::Allow // auto-accept bash too in this mode
            } else {
                PermissionDecision::Ask
            }
        }
        PermissionMode::Default => {
            match level {
                PermissionLevel::None | PermissionLevel::ReadOnly => PermissionDecision::Allow,
                PermissionLevel::Write | PermissionLevel::Execute => PermissionDecision::Ask,
                PermissionLevel::Dangerous => PermissionDecision::Ask,
                PermissionLevel::Forbidden => PermissionDecision::Deny,
            }
        }
    }
}

/// Classify a bash command's risk level.
pub fn classify_bash_command(command: &str) -> PermissionLevel {
    let lower = command.to_ascii_lowercase();

    // Forbidden: catastrophically destructive
    let forbidden = ["rm -rf /", ":(){ :|:& };:", "mkfs.", "dd if=/dev/zero of=/dev/sd"];
    if forbidden.iter().any(|p| lower.contains(p)) {
        return PermissionLevel::Forbidden;
    }

    // Dangerous: destructive or security-sensitive
    let dangerous = [
        "rm -rf", "rm -r", "chmod 777", "chown", "shutdown", "reboot", "halt",
        "DROP TABLE", "TRUNCATE TABLE", "DELETE FROM",
        "curl.*|.*sh", "wget.*|.*sh", // piped downloads
        "git push --force", "git reset --hard",
        "passwd", "userdel", "useradd",
    ];
    if dangerous.iter().any(|p| lower.contains(&p.to_lowercase())) {
        return PermissionLevel::Dangerous;
    }

    // Read-only: safe commands
    let readonly = [
        "cat ", "ls", "grep ", "find ", "head ", "tail ", "pwd", "echo ", "whoami",
        "date", "uname", "df ", "du ", "wc ", "which ", "env", "printenv",
        "git status", "git log", "git diff", "git branch", "git show",
        "cargo check", "cargo test", "cargo build", "cargo clippy",
        "npm test", "npm run lint", "npx ",
    ];
    if readonly.iter().any(|p| lower.starts_with(p) || lower.starts_with(&format!(" {p}"))) {
        return PermissionLevel::ReadOnly;
    }

    // Default: treat as Execute
    PermissionLevel::Execute
}

/// Detect secrets in tool output.
pub fn detect_secrets(output: &str) -> Vec<String> {
    let mut found = Vec::new();

    let patterns = [
        ("AWS Access Key", "AKIA"),
        ("GitHub Token", "ghp_"),
        ("GitHub Token", "gho_"),
        ("Slack Token", "xoxb-"),
        ("Slack Token", "xoxp-"),
        ("Bearer Token", "Bearer ey"),
        ("Private Key", "-----BEGIN RSA PRIVATE KEY"),
        ("Private Key", "-----BEGIN OPENSSH PRIVATE KEY"),
        ("Private Key", "-----BEGIN EC PRIVATE KEY"),
    ];

    for (name, pattern) in &patterns {
        if output.contains(pattern) {
            found.push(format!("Possible {name} detected in output"));
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_paths() {
        assert!(is_blocked_path("/home/user/.env"));
        assert!(is_blocked_path("project/.env.local"));
        assert!(is_blocked_path("/etc/secrets.yaml"));
        assert!(is_blocked_path("cert.pem"));
        assert!(!is_blocked_path("src/main.rs"));
        assert!(!is_blocked_path("README.md"));
    }

    #[test]
    fn bash_classification() {
        assert_eq!(classify_bash_command("ls -la"), PermissionLevel::ReadOnly);
        assert_eq!(classify_bash_command("cat file.txt"), PermissionLevel::ReadOnly);
        assert_eq!(classify_bash_command("cargo test"), PermissionLevel::ReadOnly);
        assert_eq!(classify_bash_command("rm -rf /"), PermissionLevel::Forbidden);
        assert_eq!(classify_bash_command("rm -rf ./build"), PermissionLevel::Dangerous);
        assert_eq!(classify_bash_command("mkdir -p /tmp/test"), PermissionLevel::Execute);
    }

    #[test]
    fn secret_detection() {
        assert!(!detect_secrets("normal output").is_empty() == false);
        assert!(!detect_secrets("AKIAIOSFODNN7EXAMPLE").is_empty());
        assert!(!detect_secrets("ghp_xxxxxxxxxxxx").is_empty());
    }

    #[test]
    fn permission_modes() {
        let default_cfg = PermissionConfig::default();
        assert_eq!(check_permission("Read", PermissionLevel::ReadOnly, &default_cfg), PermissionDecision::Allow);
        assert_eq!(check_permission("Bash", PermissionLevel::Execute, &default_cfg), PermissionDecision::Ask);

        let bypass_cfg = PermissionConfig { mode: PermissionMode::Bypass, ..Default::default() };
        assert_eq!(check_permission("Bash", PermissionLevel::Execute, &bypass_cfg), PermissionDecision::Allow);

        let plan_cfg = PermissionConfig { mode: PermissionMode::Plan, ..Default::default() };
        assert_eq!(check_permission("Write", PermissionLevel::Write, &plan_cfg), PermissionDecision::Deny);
    }
}
