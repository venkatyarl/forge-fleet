//! Global panic hook that captures panics into an in-memory queue.
//! The heartbeat collector drains it per-beat so panics flow into
//! `beat.encountered_bugs[]` → materializer → `fleet_bug_reports`
//! → leader's `self_heal_tick` (per `self-heal-coordination.md`).
//!
//! Motivation: `feedback_ff_supervise_utf8_panic.md`. The ff-terminal
//! byte-slice panic silently turned successful supervise runs into
//! failures with no reporting. With this hook, the daemon's own panics
//! propagate to the leader within one pulse tick.

use std::panic;
use std::sync::{Mutex, OnceLock};

use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct CapturedBug {
    pub signature: String,
    pub file_path: Option<String>,
    pub line_number: Option<u32>,
    pub error_class: String,
    pub stack_excerpt: Option<String>,
    pub binary_version: Option<String>,
    pub tier: String,
}

static QUEUE: OnceLock<Mutex<Vec<CapturedBug>>> = OnceLock::new();

fn queue() -> &'static Mutex<Vec<CapturedBug>> {
    QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Install the global panic hook. Call once at daemon startup. The prior
/// hook is preserved and chained (so cargo's default panic print still
/// runs, which is what the user sees in terminals).
pub fn install() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let loc = info.location();
        let file_path = loc.map(|l| l.file().to_string());
        let line_number = loc.map(|l| l.line());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("unknown panic");
        let error_class = classify_panic(msg);
        let signature = compute_signature(&file_path, line_number, &error_class);
        let bug = CapturedBug {
            signature,
            file_path,
            line_number,
            error_class,
            stack_excerpt: Some(msg.chars().take(500).collect()),
            binary_version: option_env!("CARGO_PKG_VERSION").map(|s| s.to_string()),
            tier: "T1".to_string(),
        };
        if let Ok(mut q) = queue().lock() {
            q.push(bug);
        }
        default_hook(info);
    }));
}

/// Coarse taxonomy used as part of the bug signature. Fleet-wide dedup
/// keys on (file:line:error_class), so consistent error_class strings
/// matter — if two daemons hit the same panic, their signatures should
/// collide on the leader's queue.
fn classify_panic(msg: &str) -> String {
    if msg.contains("is not a char boundary") {
        "panic:str_index".into()
    } else if msg.contains("index out of bounds") {
        "panic:index_oob".into()
    } else if msg.contains("unwrap") && (msg.contains("None") || msg.contains("Err")) {
        "panic:unwrap".into()
    } else if msg.contains("overflow") {
        "panic:overflow".into()
    } else if msg.contains("divide by zero") {
        "panic:divide_by_zero".into()
    } else if msg.contains("assertion failed") {
        "panic:assert".into()
    } else {
        "panic:generic".into()
    }
}

fn compute_signature(file: &Option<String>, line: Option<u32>, class: &str) -> String {
    let mut h = Sha256::new();
    h.update(file.as_deref().unwrap_or("?"));
    h.update(b":");
    h.update(line.map(|l| l.to_string()).unwrap_or_else(|| "?".into()));
    h.update(b":");
    h.update(class);
    let full = format!("{:x}", h.finalize());
    full.chars().take(16).collect()
}

/// Drain all queued bugs. Called by heartbeat collector each tick.
/// Returns the bugs and leaves the queue empty.
pub fn drain() -> Vec<CapturedBug> {
    queue()
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

/// Record a panic captured from a subprocess's stderr (e.g. a cargo
/// invocation that panicked inside an agent's Bash tool call). The
/// signature key uses the source_hint in place of file:line since we
/// don't have proper stack info.
pub fn record_external_panic(msg: &str, source_hint: Option<&str>) {
    let error_class = classify_panic(msg);
    let signature = compute_signature(&source_hint.map(|s| s.to_string()), None, &error_class);
    let bug = CapturedBug {
        signature,
        file_path: source_hint.map(|s| s.to_string()),
        line_number: None,
        error_class,
        stack_excerpt: Some(msg.chars().take(500).collect()),
        binary_version: option_env!("CARGO_PKG_VERSION").map(|s| s.to_string()),
        tier: "T1".to_string(),
    };
    if let Ok(mut q) = queue().lock() {
        q.push(bug);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_recognizes_common_panics() {
        assert_eq!(
            classify_panic("byte index 500 is not a char boundary"),
            "panic:str_index"
        );
        assert_eq!(
            classify_panic("index out of bounds: the len is 3 but the index is 5"),
            "panic:index_oob"
        );
        assert_eq!(
            classify_panic("called `Option::unwrap()` on a `None` value"),
            "panic:unwrap"
        );
    }

    #[test]
    fn signature_stable_across_inputs() {
        let a = compute_signature(&Some("src/main.rs".into()), Some(1534), "panic:str_index");
        let b = compute_signature(&Some("src/main.rs".into()), Some(1534), "panic:str_index");
        assert_eq!(a, b);
    }

    #[test]
    fn drain_empties_queue() {
        record_external_panic("test panic msg", Some("test_source"));
        assert!(!drain().is_empty());
        assert!(drain().is_empty());
    }
}
