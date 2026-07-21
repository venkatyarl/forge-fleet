//! Side-effect-free, per-OS detection of installed command-line software.

use std::collections::HashMap;
use std::process::{Command, Stdio};

/// Detect installed versions for command or package names.
///
/// Names are passed directly as process arguments (never through a shell). Invalid
/// package identifiers are ignored. The returned map contains only software that
/// could be located and whose version could be parsed.
pub fn detect_installed_versions<I, S>(software: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    software
        .into_iter()
        .filter_map(|name| {
            let name = name.as_ref().trim();
            if !valid_name(name) {
                return None;
            }
            detect_one(name).map(|version| (name.to_owned(), version))
        })
        .collect()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

fn output(command: &str, args: &[&str]) -> Option<String> {
    let result = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .ok()?;
    if !result.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    let text = if stdout.trim().is_empty() {
        stderr.as_ref()
    } else {
        stdout.as_ref()
    };
    (!text.trim().is_empty()).then(|| text.trim().to_owned())
}

#[cfg(not(target_os = "windows"))]
fn detect_one(name: &str) -> Option<String> {
    // Avoid running an arbitrary same-named process unless the OS can resolve it.
    if output("which", &[name]).is_some() {
        for flag in ["--version", "-V", "version"] {
            if let Some(raw) = output(name, &[flag])
                && let Some(version) = parse_version(&raw)
            {
                return Some(version);
            }
        }
    }

    #[cfg(target_os = "macos")]
    if let Some(raw) = output("brew", &["info", "--json=v2", name]) {
        return parse_brew_version(&raw);
    }

    None
}

#[cfg(target_os = "windows")]
fn detect_one(name: &str) -> Option<String> {
    // `where.exe` and winget receive an argument vector, so metacharacters cannot
    // turn a package name into a command.
    if output("where.exe", &[name]).is_some() {
        for flag in ["--version", "-V", "version"] {
            if let Some(raw) = output(name, &[flag])
                && let Some(version) = parse_version(&raw)
            {
                return Some(version);
            }
        }
    }
    output(
        "winget.exe",
        &[
            "list",
            "--id",
            name,
            "--exact",
            "--accept-source-agreements",
        ],
    )
    .and_then(|raw| parse_winget_version(&raw, name))
}

fn parse_version(raw: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        line.split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | '(' | ')'))
            .map(|token| token.trim_matches(|ch: char| matches!(ch, 'v' | 'V' | ':' | '=')))
            .find(|token| {
                !token.is_empty()
                    && token.len() <= 64
                    && token.chars().any(|ch| ch.is_ascii_digit())
                    && token
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ".+-_".contains(ch))
            })
            .map(str::to_owned)
    })
}

#[cfg(target_os = "macos")]
fn parse_brew_version(raw: &str) -> Option<String> {
    let document: serde_json::Value = serde_json::from_str(raw).ok()?;
    let package = document
        .get("formulae")
        .and_then(serde_json::Value::as_array)
        .and_then(|items| items.first())
        .or_else(|| {
            document
                .get("casks")
                .and_then(serde_json::Value::as_array)
                .and_then(|items| items.first())
        })?;
    package
        .pointer("/installed/0/version")
        .or_else(|| package.get("installed"))
        .and_then(|value| {
            value.as_str().or_else(|| {
                value
                    .as_array()
                    .and_then(|items| items.first())
                    .and_then(serde_json::Value::as_str)
            })
        })
        .map(str::to_owned)
}

#[cfg(target_os = "windows")]
fn parse_winget_version(raw: &str, name: &str) -> Option<String> {
    raw.lines()
        .filter(|line| {
            line.to_ascii_lowercase()
                .contains(&name.to_ascii_lowercase())
        })
        .find_map(parse_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_version_output() {
        assert_eq!(
            parse_version("git version 2.49.0\n").as_deref(),
            Some("2.49.0")
        );
        assert_eq!(
            parse_version("tool v1.2.3-beta+4 (build)\n").as_deref(),
            Some("1.2.3-beta+4")
        );
    }

    #[test]
    fn rejects_unsafe_names_without_execution() {
        let versions = detect_installed_versions(["; touch /tmp/nope", "", "a/b"]);
        assert!(versions.is_empty());
    }
}
