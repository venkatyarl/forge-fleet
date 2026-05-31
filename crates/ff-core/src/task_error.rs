//! Structured error classification for failed fleet tasks.
//!
//! Today a failed `fleet_tasks` row carries only raw text: the result JSON
//! (`{exit, stdout, stderr}`) plus a free-form `error` string. That is not
//! triageable at a glance, and the self-healing pipeline has nothing
//! structured to branch on. This module is a **pure, deterministic
//! classifier**: it maps a failed task's already-stored output to a small,
//! well-defined [`TaskErrorClass`].
//!
//! Deliberately storage-free: the class is derived on the fly from data the
//! worker already persists (see `ff_agent::task_runner::run_shell` writing
//! `{exit, stdout, stderr}` and the `WHERE status = 'running'` failure
//! branch). Persisting the class is a separate, operator-directed follow-up
//! — there is intentionally no schema change here.
//!
//! Matching is case-insensitive, allocation-light (we lowercase the inputs
//! once), and **order-sensitive**: the most specific classes are checked
//! first so e.g. an OOM kill (`exit 137` + "killed") is not mis-bucketed as a
//! generic `Cancelled`/`Unknown`, and an LLM-port refusal is preferred over a
//! generic SSH "connection refused".

/// A small, well-defined taxonomy of fleet-task failure causes.
///
/// The string forms ([`TaskErrorClass::as_str`]) are stable identifiers
/// suitable for logging and for a future persisted column; the
/// [`TaskErrorClass::hint`] is a one-line operator-facing remediation note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskErrorClass {
    /// The target host could not be reached or authenticated over SSH:
    /// connection refused, no route, link timeout, host-key mismatch, or a
    /// publickey auth rejection.
    SshUnreachable,
    /// A compile/build step failed — rustc/cargo errors, link errors.
    BuildFailed,
    /// The process (or a child of it) ran out of memory / was OOM-killed.
    OutOfMemory,
    /// The filesystem or quota is full.
    DiskFull,
    /// An LLM inference endpoint was missing or unhealthy (model not loaded,
    /// no healthy endpoint, 503, refused on an inference port).
    ModelUnavailable,
    /// The task exceeded its deadline / a timeout fired.
    Timeout,
    /// A permission/privilege problem (EACCES, must-be-root, denied).
    PermissionDenied,
    /// A referenced command or file did not exist (command not found, no such
    /// file, exit 127).
    NotFound,
    /// The task was cancelled (operator cancel or an explicit cancellation).
    Cancelled,
    /// Could not be classified — fall back to reading the raw output.
    Unknown,
}

impl TaskErrorClass {
    /// Stable machine identifier for this class.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskErrorClass::SshUnreachable => "ssh_unreachable",
            TaskErrorClass::BuildFailed => "build_failed",
            TaskErrorClass::OutOfMemory => "out_of_memory",
            TaskErrorClass::DiskFull => "disk_full",
            TaskErrorClass::ModelUnavailable => "model_unavailable",
            TaskErrorClass::Timeout => "timeout",
            TaskErrorClass::PermissionDenied => "permission_denied",
            TaskErrorClass::NotFound => "not_found",
            TaskErrorClass::Cancelled => "cancelled",
            TaskErrorClass::Unknown => "unknown",
        }
    }

    /// One-line operator-facing remediation hint.
    pub fn hint(&self) -> &'static str {
        match self {
            TaskErrorClass::SshUnreachable => {
                "host unreachable/auth failed — check power, IP, and SSH keys (ff fleet health)"
            }
            TaskErrorClass::BuildFailed => {
                "compile error — read stderr; fix the source or pin toolchain, then redispatch"
            }
            TaskErrorClass::OutOfMemory => {
                "ran out of RAM — unload a model on the target to free memory, then retry"
            }
            TaskErrorClass::DiskFull => {
                "disk/quota full — free space (ff model delete / clean caches) and retry"
            }
            TaskErrorClass::ModelUnavailable => {
                "LLM endpoint down — load the model (ff model load) or route to another node"
            }
            TaskErrorClass::Timeout => {
                "exceeded deadline — raise --timeout or split the work into smaller tasks"
            }
            TaskErrorClass::PermissionDenied => {
                "permission denied — check sudo/ownership; Taylor lacks passwordless sudo by design"
            }
            TaskErrorClass::NotFound => {
                "command/file missing — install the tool or fix the path, then redispatch"
            }
            TaskErrorClass::Cancelled => "task was cancelled — re-add it if it should still run",
            TaskErrorClass::Unknown => "unclassified — inspect the raw stderr/stdout below",
        }
    }
}

/// True when `code` is 137 (= 128 + 9, SIGKILL) — the classic
/// OOM-killer / `kill -9` exit status.
fn is_sigkill_exit(code: Option<i64>) -> bool {
    code == Some(137)
}

/// Classify a failed fleet task from its already-stored output.
///
/// Inputs are taken directly from the persisted `fleet_tasks` row: `stderr`
/// and `stdout` come from the result JSON (`{exit, stdout, stderr}`),
/// `exit_code` is that JSON's `exit` field, and the free-form `error` column
/// (e.g. `"task exceeded max duration of 600s"` or `"non-zero exit: N"`) is
/// appended to the scanned text so runner-level failures are also classified.
///
/// Pure and deterministic. Order matters: the most specific signatures win.
pub fn classify_task_error(
    stderr: &str,
    stdout: &str,
    exit_code: Option<i64>,
    error: Option<&str>,
) -> TaskErrorClass {
    // Lowercase once; scan the combined haystack. stderr first (most
    // diagnostic), then the free-form error column, then stdout as a
    // fallback for tools that log failures to stdout.
    let mut hay = String::with_capacity(stderr.len() + stdout.len() + 32);
    hay.push_str(&stderr.to_ascii_lowercase());
    hay.push(' ');
    if let Some(e) = error {
        hay.push_str(&e.to_ascii_lowercase());
        hay.push(' ');
    }
    hay.push_str(&stdout.to_ascii_lowercase());
    let h = hay.as_str();
    let has = |needle: &str| h.contains(needle);

    // 1. OOM — most specific. Check before generic "killed"/Cancelled/Unknown.
    //    Either an explicit OOM message, or a SIGKILL exit (137) paired with a
    //    "killed" marker (bare 137 alone is ambiguous, so require the word).
    if has("out of memory")
        || has("oom-kill")
        || has("oom killer")
        || has("cannot allocate memory")
        || has("memoryerror")
        || (is_sigkill_exit(exit_code) && (has("killed") || has("signal: 9")))
    {
        return TaskErrorClass::OutOfMemory;
    }

    // 2. Disk full — specific, before the generic permission catch.
    if has("no space left on device") || has("disk quota exceeded") || has("enospc") {
        return TaskErrorClass::DiskFull;
    }

    // 3. Model/LLM endpoint unavailable — must precede SshUnreachable so an
    //    inference-port refusal is bucketed as the LLM being down, not a host
    //    being offline. Keyed on LLM-specific markers OR a refusal that also
    //    mentions an inference/model context.
    if has("no healthy endpoint")
        || has("model not found")
        || has("no such model")
        || has("failed to load model")
        || has("503 service unavailable")
        || has("http 503")
        || (has("connection refused")
            && (has("llm")
                || has("inference")
                || has("llama-server")
                || has("mlx_lm")
                || has("vllm")
                || has("/v1/")
                || has("completions")))
    {
        return TaskErrorClass::ModelUnavailable;
    }

    // 4. SSH unreachable / auth. "connection timed out" here is
    //    connection-level; the deadline Timeout class below keys on
    //    task-duration phrasing.
    if has("no route to host")
        || has("host key verification")
        || has("permission denied (publickey")
        || has("connection refused")
        || has("connection timed out")
        || has("ssh: connect to host")
        || has("could not resolve hostname")
    {
        return TaskErrorClass::SshUnreachable;
    }

    // 5. Build/compile failure.
    if has("error[e")
        || has("could not compile")
        || has("error: linking with")
        || has("ld: symbol")
        || has("undefined reference")
        || has("cargo build")
        || (has("rustc") && has("error"))
    {
        return TaskErrorClass::BuildFailed;
    }

    // 6. Timeout / deadline — task-duration phrasing (runner writes
    //    "task exceeded max duration of Ns" into `error`).
    if has("exceeded max duration")
        || has("deadline exceeded")
        || has("timed out")
        || has("timeout")
    {
        return TaskErrorClass::Timeout;
    }

    // 7. NotFound — "command not found" / "no such file" / exit 127.
    if has("command not found")
        || has("no such file or directory")
        || has("not found in $path")
        || (exit_code == Some(127) && !has("permission"))
    {
        return TaskErrorClass::NotFound;
    }

    // 8. Permission denied (after SSH-publickey, which is more specific).
    if has("permission denied")
        || has("eacces")
        || has("operation not permitted")
        || has("must be run as root")
        || has("must be root")
        || has("are you root")
    {
        return TaskErrorClass::PermissionDenied;
    }

    // 9. Cancellation.
    if has("cancelled") || has("canceled") || has("task was cancelled") {
        return TaskErrorClass::Cancelled;
    }

    TaskErrorClass::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(stderr: &str, exit: Option<i64>) -> TaskErrorClass {
        classify_task_error(stderr, "", exit, None)
    }

    #[test]
    fn ssh_unreachable() {
        assert_eq!(
            classify(
                "ssh: connect to host 192.168.5.108 port 22: Connection refused",
                None
            ),
            TaskErrorClass::SshUnreachable
        );
        assert_eq!(
            classify("ssh: No route to host", None),
            TaskErrorClass::SshUnreachable
        );
        assert_eq!(
            classify("Permission denied (publickey,password).", None),
            TaskErrorClass::SshUnreachable
        );
        assert_eq!(
            classify("Host key verification failed.", None),
            TaskErrorClass::SshUnreachable
        );
    }

    #[test]
    fn build_failed() {
        assert_eq!(
            classify("error[E0432]: unresolved import `foo::bar`", None),
            TaskErrorClass::BuildFailed
        );
        assert_eq!(
            classify(
                "error: could not compile `ff-terminal` due to 3 previous errors",
                None
            ),
            TaskErrorClass::BuildFailed
        );
    }

    #[test]
    fn out_of_memory_wins_over_cancelled_and_signal() {
        // Explicit OOM message.
        assert_eq!(
            classify("fatal runtime error: Cannot allocate memory", None),
            TaskErrorClass::OutOfMemory
        );
        // SIGKILL (137) + "Killed" must be OOM, not a generic bucket.
        assert_eq!(classify("Killed", Some(137)), TaskErrorClass::OutOfMemory);
    }

    #[test]
    fn disk_full() {
        assert_eq!(
            classify("write error: No space left on device", None),
            TaskErrorClass::DiskFull
        );
        assert_eq!(
            classify("rsync: disk quota exceeded (122)", None),
            TaskErrorClass::DiskFull
        );
    }

    #[test]
    fn model_unavailable_beats_ssh_refused() {
        assert_eq!(
            classify("no healthy endpoint for model qwen3-coder", None),
            TaskErrorClass::ModelUnavailable
        );
        // A refusal on an inference port is the LLM being down, not the host.
        assert_eq!(
            classify(
                "llama-server /v1/chat/completions: Connection refused",
                None
            ),
            TaskErrorClass::ModelUnavailable
        );
        assert_eq!(
            classify("HTTP 503 Service Unavailable from inference backend", None),
            TaskErrorClass::ModelUnavailable
        );
    }

    #[test]
    fn timeout() {
        // The runner's own phrasing.
        assert_eq!(
            classify_task_error("", "", None, Some("task exceeded max duration of 600s")),
            TaskErrorClass::Timeout
        );
        assert_eq!(
            classify("context deadline exceeded", None),
            TaskErrorClass::Timeout
        );
    }

    #[test]
    fn permission_denied() {
        assert_eq!(
            classify("EACCES: permission denied, open '/etc/hosts'", None),
            TaskErrorClass::PermissionDenied
        );
        assert_eq!(
            classify("apt-get: you must be root to perform this operation", None),
            TaskErrorClass::PermissionDenied
        );
    }

    #[test]
    fn not_found() {
        assert_eq!(
            classify("bash: cargo: command not found", Some(127)),
            TaskErrorClass::NotFound
        );
        assert_eq!(
            classify("cat: /tmp/missing: No such file or directory", None),
            TaskErrorClass::NotFound
        );
        // Bare exit 127 with no other signal.
        assert_eq!(classify("", Some(127)), TaskErrorClass::NotFound);
    }

    #[test]
    fn cancelled() {
        assert_eq!(
            classify_task_error("", "", None, Some("task was cancelled by operator")),
            TaskErrorClass::Cancelled
        );
    }

    #[test]
    fn unknown_fallback() {
        assert_eq!(
            classify("some unrecognized failure mode", Some(1)),
            TaskErrorClass::Unknown
        );
    }

    #[test]
    fn empty_input_is_unknown() {
        assert_eq!(
            classify_task_error("", "", None, None),
            TaskErrorClass::Unknown
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(
            classify("CONNECTION REFUSED", None),
            TaskErrorClass::SshUnreachable
        );
        assert_eq!(
            classify("NO SPACE LEFT ON DEVICE", None),
            TaskErrorClass::DiskFull
        );
    }

    #[test]
    fn as_str_and_hint_are_nonempty() {
        for c in [
            TaskErrorClass::SshUnreachable,
            TaskErrorClass::BuildFailed,
            TaskErrorClass::OutOfMemory,
            TaskErrorClass::DiskFull,
            TaskErrorClass::ModelUnavailable,
            TaskErrorClass::Timeout,
            TaskErrorClass::PermissionDenied,
            TaskErrorClass::NotFound,
            TaskErrorClass::Cancelled,
            TaskErrorClass::Unknown,
        ] {
            assert!(!c.as_str().is_empty());
            assert!(!c.hint().is_empty());
        }
    }
}
