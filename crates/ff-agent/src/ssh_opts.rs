//! Canonical SSH options for **daemon-spawned** connections.
//!
//! ## Why this module exists (HA.2, the wedged-agent class)
//!
//! Every SSH the daemon (`forgefleetd`) spawns inherits `SSH_AUTH_SOCK` from
//! its environment. On headless Ubuntu hosts (sophie/priya) that socket points
//! at a gnome-keyring ssh-agent that can wedge: it accepts the connection but
//! blocks forever on the sign request. `ConnectTimeout` only covers the TCP
//! connect, so the ssh process hangs at *auth* and the task that spawned it
//! sits `running` until its (often long) duration cap.
//!
//! This bit us twice on 2026-06-14: first the HA backup rsync (#304), then the
//! wave-upgrade restart SSH (#305) — a hung `restart on sophie` held the
//! auto-upgrade singleton for 53 min so NO host could upgrade, the very
//! pipeline that ships fixes strangled by the agent it inherited. Those two
//! sites were patched inline; this module generalizes the fix to the ~10 other
//! daemon SSH/rsync call sites (mesh_check, revive, model_transfer, oauth
//! distribution, pg_failover, conformance, ssh_key_manager, openclaw,
//! verify_computer, panic_stop) that were latent on the same hosts.
//!
//! ## The fix
//!
//! - `IdentityAgent=none` makes ssh ignore the inherited agent socket entirely
//!   and use the on-disk key instead. Fleet keys are always materialized on
//!   disk (DB → `ff github sync` at enrollment), so this never costs us auth —
//!   it only removes the hang.
//! - `BatchMode=yes` keeps it non-interactive: no password/passphrase prompt
//!   that would itself hang a daemon-spawned ssh forever.
//!
//! Use [`SSH_AGENT_BYPASS`] for ssh/rsync commands embedded in a shell string
//! (heredocs, `GIT_SSH_COMMAND`, `rsync -e 'ssh …'`); use [`ssh_bypass_args`]
//! for `Command::new("ssh").args(…)` argv construction.

/// The agent-bypass options as a single shell-string fragment, e.g. for
/// heredoc playbooks, `GIT_SSH_COMMAND='ssh … '`, or `rsync -e 'ssh … '`.
///
/// Drop-in replacement for a bare `-o BatchMode=yes` fragment.
pub const SSH_AGENT_BYPASS: &str = "-o IdentityAgent=none -o BatchMode=yes";

/// The agent-bypass options as discrete `-o KEY=VAL` argv tokens, for
/// `Command::new("ssh").args(ssh_bypass_args())`.
///
/// Drop-in replacement for an inline `["-o", "BatchMode=yes"]` pair.
pub const fn ssh_bypass_args() -> [&'static str; 4] {
    ["-o", "IdentityAgent=none", "-o", "BatchMode=yes"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_and_argv_forms_agree() {
        // Both forms must carry the two options that defeat the wedged agent.
        assert!(SSH_AGENT_BYPASS.contains("IdentityAgent=none"));
        assert!(SSH_AGENT_BYPASS.contains("BatchMode=yes"));
        let args = ssh_bypass_args();
        assert!(args.contains(&"IdentityAgent=none"));
        assert!(args.contains(&"BatchMode=yes"));
        // argv form must alternate -o / value so it survives word-splitting.
        assert_eq!(args[0], "-o");
        assert_eq!(args[2], "-o");
    }
}
