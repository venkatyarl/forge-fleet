//! Per-tool upgrade playbook snippets.

/// Resolve a tool's upgrade playbook for a given OS family.
///
/// Tries an exact `(tool, os_family)` match first (so specialised arms like
/// `linux-dgx` win), then falls back to the base family (`linux-ubuntu` →
/// `linux`, `macos-15` → `macos`). Without the fallback, every host whose
/// `os_family` is a sub-family (e.g. `linux-ubuntu`) failed with
/// "no playbook for os=linux-ubuntu" — the defer-worker / `ff daemon`
/// upgrade path (which uses this fn) couldn't build forgefleetd/ff on any
/// Linux host, stalling fleet self-upgrade. The DB-driven `auto_upgrade`
/// path already did this via its own `base_family`; this mirrors it.
/// (2026-05-31.)
pub fn playbook_for(tool: &str, os_family: &str) -> Option<String> {
    if let Some(p) = playbook_exact(tool, os_family) {
        return Some(p);
    }
    match base_family(os_family) {
        Some(base) if base != os_family => playbook_exact(tool, base),
        _ => None,
    }
}

/// Build a shell snippet that installs a freshly-built `target/release/{bin}`
/// to `{dest}` **atomically** and only after proving the result runs.
///
/// Why this exists: a plain `install -m 755 target/release/ff $DEST` writes
/// straight into PATH, so a disk-full / interrupted copy leaves a truncated,
/// unrunnable binary *there*. Observed on ace 2026-06-14: a 304-byte garbage
/// `~/.local/bin/ff` from an ENOSPC `install` (the disk was at 100%). Every
/// `ff` invocation then died with a shell syntax error, the host could not run
/// any `ff` verb, and — because forgefleetd kept heartbeating on its stale
/// binary — nothing detected the CLI was dead. The `&&` chain that followed
/// (`codesign`, restart) aborted, so the upgrade task "failed" yet still left
/// the poisoned binary in PATH.
///
/// This installs to `{dest}.new`, code-signs it (macOS, so the temp itself is
/// validatable), proves it executes via `--version`, then atomically renames
/// it over `{dest}`. `mv` within one filesystem is a single rename(2), so PATH
/// only ever sees the old (working) binary or the new (validated) one — never a
/// half-written one. On ANY failure the temp is removed and the snippet
/// `exit 1`s, so the upgrade is recorded as FAILED (loud + retryable by the
/// version-drift machinery) instead of silently bricking the host's CLI.
pub fn atomic_install_cmd(bin: &str, dest: &str, codesign: bool) -> String {
    let sign = if codesign {
        format!("codesign --force --sign - \"{dest}.new\" && ")
    } else {
        String::new()
    };
    format!(
        "{{ install -m 755 target/release/{bin} \"{dest}.new\" && \
         {sign}\"{dest}.new\" --version >/dev/null 2>&1 && \
         {{ [ ! -f \"{dest}\" ] || cp -f \"{dest}\" \"{dest}.prev\"; }} && \
         mv -f \"{dest}.new\" \"{dest}\"; }} || \
         {{ rm -f \"{dest}.new\"; \
         echo \"upgrade: install/validate of {dest} failed; kept existing binary\" >&2; \
         exit 1; }}"
    )
}

/// Normalise an `os_family` (e.g. `linux-ubuntu`, `linux-dgx`, `macos-26`) to its
/// base family (`linux`/`macos`/`windows`), or `None` if unrecognised. Shared with
/// `auto_upgrade` (single source of truth) so the playbook resolver and the wave
/// dispatcher can never disagree on what counts as "the linux key" — a divergence
/// there once skipped every Linux target with `no playbook key for os='linux-ubuntu'`.
pub(crate) fn base_family(os_family: &str) -> Option<&'static str> {
    if os_family.starts_with("linux") {
        Some("linux")
    } else if os_family.starts_with("macos") {
        Some("macos")
    } else if os_family.starts_with("windows") {
        Some("windows")
    } else {
        None
    }
}

/// Robust repo-sync prelude for every forge-fleet upgrade playbook.
///
/// Replaces the fragile `git pull --ff-only`, which dies
/// `fatal: Cannot fast-forward to multiple branches` (and `Not possible to
/// fast-forward, aborting`) the moment the local checkout has diverged from
/// origin — a force-pushed/rebased history upstream, a stray local commit, a
/// multi-merge-head FETCH_HEAD, or a detached HEAD. Because the pull fails the
/// build never runs, the binary never updates, and the auto-upgrade tick
/// re-queues the SAME upgrade every cycle: 748 `Cannot fast-forward` failures
/// in 24h on marcus+logan alone (the #1 deferred-task failure fleet-wide).
///
/// `git fetch && git reset --hard origin/main` lands EXACTLY on the upgrade
/// target regardless of the prior tree state — idempotent and divergence-proof.
/// This is the same approach the two paths that DON'T fail already use:
/// `ff fleet deploy` and the leader self-upgrade (auto_upgrade.rs both do
/// `git fetch origin` + `git reset --hard <ref>`). The `git clean` drops build
/// artifacts (graphify-out / node-compile-cache) that could shadow the fresh
/// tree. Fleet worker checkouts are pure deployments (Vinny, the only dev
/// tree, is excluded from auto-upgrade), so a hard reset never clobbers work.
const GIT_SYNC_FORGE_FLEET: &str = "cd ~/projects/forge-fleet && git fetch origin --prune && \
     git reset --hard origin/main && git clean -fdx graphify-out node-compile-cache";

/// Build the web console (web-forge-fleet) static export before cargo.
/// ff-gateway rust-embeds `web-forge-fleet/out` at compile time, and `out/`
/// is git-ignored, so a fresh `git reset --hard` checkout does NOT contain
/// it (the old `dashboard/dist` was committed, which is why this step was
/// never needed before the 2026-08-03 web consolidation). Operator
/// directive: never serve the placeholder — fail the upgrade if npm exists
/// but the web build fails; only warn when npm is genuinely absent.
/// Runs from the forge-fleet checkout root (GIT_SYNC_FORGE_FLEET cd's there).
const WEB_BUILD_STEP: &str = "( if command -v npm >/dev/null 2>&1; then \
     cd web-forge-fleet && npm ci --no-audit --no-fund --silent && npm run build --silent \
       || { echo \"WEB_BUILD_FAILED: refusing to deploy a placeholder web console\" >&2; exit 11; }; \
   else echo \"WARN: npm not found — web-forge-fleet/out may be stale/placeholder\" >&2; fi )";

/// Resolve the MCP listener port on the target without embedding a port in the
/// upgrade playbook.  Local `fleet.toml` values are authoritative when present;
/// `fleet_secrets.port.mcp` remains the fleet-wide fallback used by hosts whose
/// older config predates `[mcp.forgefleet]`.  Every available source must agree.
///
/// This exact program is also executed by the unit tests.  Keep string literals
/// double-quoted so it can be safely single-quoted in the generated shell.
const MCP_PORT_RESOLVER_PY: &str = r#"import pathlib
import sys
import tomllib
import urllib.parse

def fail(message):
    raise SystemExit("upgrade: MCP port authority " + message)

def checked_port(value, source):
    if isinstance(value, bool):
        fail("is invalid at " + source)
    try:
        number = int(value)
    except (TypeError, ValueError):
        fail("is invalid at " + source)
    if str(value).strip() != str(number) or not 1 <= number <= 65535:
        fail("is invalid at " + source)
    return number

config_path = pathlib.Path(sys.argv[1])
db_value = sys.argv[2].strip()
sources = []
if config_path.exists():
    try:
        config = tomllib.loads(config_path.read_text())
    except Exception as error:
        fail("cannot parse fleet.toml: " + type(error).__name__)
    mcp_root = config.get("mcp") or {}
    if not isinstance(mcp_root, dict):
        fail("has a non-table mcp section")
    mcp = mcp_root.get("forgefleet") or {}
    if not isinstance(mcp, dict):
        fail("has a non-table mcp.forgefleet section")
    endpoint = mcp.get("endpoint", mcp.get("url"))
    if endpoint is not None:
        raw_endpoint = str(endpoint).strip()
        parsed = urllib.parse.urlsplit(raw_endpoint if "://" in raw_endpoint else "http://" + raw_endpoint)
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            fail("has an invalid mcp.forgefleet.endpoint")
        if parsed.path.rstrip("/") not in ("", "/mcp") or parsed.query or parsed.fragment:
            fail("has an invalid MCP endpoint path")
        try:
            endpoint_port = parsed.port
        except ValueError:
            fail("has an invalid mcp.forgefleet.endpoint port")
        sources.append(("mcp.forgefleet.endpoint", endpoint_port or (443 if parsed.scheme == "https" else 80)))
    if mcp.get("port") is not None:
        sources.append(("mcp.forgefleet.port", checked_port(mcp.get("port"), "mcp.forgefleet.port")))
    ports = config.get("ports") or {}
    if not isinstance(ports, dict):
        fail("has a non-table ports section")
    if ports.get("forgefleet") is not None:
        sources.append(("ports.forgefleet", checked_port(ports.get("forgefleet"), "ports.forgefleet")))
if db_value:
    sources.append(("fleet_secrets.port.mcp", checked_port(db_value, "fleet_secrets.port.mcp")))
if not sources:
    fail("is missing; configure fleet.toml or fleet_secrets.port.mcp")
if len({value for _, value in sources}) != 1:
    fail("sources disagree: " + ", ".join(name for name, _ in sources))
print(sources[0][1])"#;

/// Validate the exact macOS supervisor definitions before replacing the shared
/// binary.  This deliberately rejects the old daemon-only topology: upgrading
/// such a host would make the client transport disappear with the daemon.
const MCP_PLIST_VALIDATOR_PY: &str = r#"import pathlib
import plistlib
import sys

mcp_path = pathlib.Path(sys.argv[1])
daemon_path = pathlib.Path(sys.argv[2])
binary = sys.argv[3]
port = int(sys.argv[4])

def fail(message):
    raise SystemExit("upgrade: macOS MCP topology " + message)

def load(path):
    if not path.is_file() or path.is_symlink():
        fail("requires regular non-symlink plist " + str(path))
    try:
        with path.open("rb") as handle:
            return plistlib.load(handle)
    except Exception as error:
        fail("cannot parse " + str(path) + ": " + type(error).__name__)

mcp = load(mcp_path)
daemon = load(daemon_path)
if mcp.get("Label") != "com.forgefleet.forgefleet-mcp":
    fail("has the wrong MCP label")
if daemon.get("Label") != "com.forgefleet.forgefleetd":
    fail("has the wrong daemon label")
mcp_args = mcp.get("ProgramArguments")
allowed_listens = {"0.0.0.0:" + str(port), "127.0.0.1:" + str(port), "[::]:" + str(port)}
if not isinstance(mcp_args, list) or len(mcp_args) != 4 or mcp_args[:3] != [binary, "mcp", "--listen"] or mcp_args[3] not in allowed_listens:
    fail("ProgramArguments do not identify the configured persistent HTTP MCP listener")
daemon_args = daemon.get("ProgramArguments")
plain_daemon = daemon_args == [binary, "start"]
identified_daemon = isinstance(daemon_args, list) and len(daemon_args) == 4 and daemon_args[0] == binary and daemon_args[1] in {"--node-name", "--worker-name"} and isinstance(daemon_args[2], str) and bool(daemon_args[2].strip()) and daemon_args[3] == "start"
if not (plain_daemon or identified_daemon):
    fail("daemon ProgramArguments do not identify the exact ForgeFleet daemon")"#;

/// Runs after the MCP listener has accepted a fresh JSON-RPC client.  The
/// short-lived parent forks a new session before acknowledging success, so the
/// real restart transaction survives the daemon killing its task runner.  If
/// the new daemon does not remain active, the child restores the old binary,
/// restarts both exact supervisors, and proves the old MCP transport again.
const DETACHED_DAEMON_RESTART_PY: &str = r#"import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time
import urllib.request

os_family, daemon_identity, mcp_identity, binary, old_sha, port_text, lock_dir, log_path = sys.argv[1:]
if os_family not in ("linux", "macos"):
    raise SystemExit("upgrade: unsupported detached restart OS")
port = int(port_text)
read_fd, write_fd = os.pipe()
pid = os.fork()
if pid:
    os.close(write_fd)
    ready = os.read(read_fd, 1)
    os.close(read_fd)
    if ready != b"1":
        raise SystemExit("upgrade: detached daemon restart failed to enter a new session")
    print(pid)
    raise SystemExit(0)

os.close(read_fd)
os.setsid()
log = open(log_path, "a", buffering=1)
os.dup2(log.fileno(), 1)
os.dup2(log.fileno(), 2)
os.write(write_fd, b"1")
os.close(write_fd)
safe_to_unlock = False

def run(command):
    return subprocess.run(command, stdin=subprocess.DEVNULL, stdout=log, stderr=log, check=False).returncode == 0

def service_active(identity):
    if os_family == "linux":
        return run(["systemctl", "--user", "is-active", "--quiet", identity])
    output = subprocess.run(["launchctl", "print", identity], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=log, text=True, check=False)
    return output.returncode == 0 and "state = running" in output.stdout

def restart(identity):
    if os_family == "linux":
        return run(["systemctl", "--user", "restart", identity])
    return run(["launchctl", "kickstart", "-k", identity])

def mcp_ready():
    health_url = "http://127.0.0.1:" + str(port) + "/mcp/health"
    mcp_url = "http://127.0.0.1:" + str(port) + "/mcp"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    payload = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "forgefleet-upgrade-rollback", "version": "1"}}}).encode()
    for _ in range(20):
        try:
            with opener.open(health_url, timeout=2) as response:
                healthy = response.status == 200 and response.read().strip() == b"ok"
            request = urllib.request.Request(mcp_url, data=payload, headers={"Content-Type": "application/json"})
            with opener.open(request, timeout=3) as response:
                decoded = json.loads(response.read())
            result = decoded.get("result")
            if healthy and isinstance(result, dict) and isinstance(result.get("serverInfo"), dict) and not decoded.get("error"):
                return True
        except Exception:
            pass
        time.sleep(1)
    return False

def restore_old_binary():
    source = pathlib.Path(binary + ".prev")
    staged = pathlib.Path(binary + ".rollback.new")
    if not source.is_file() or source.is_symlink():
        return False
    try:
        if hashlib.sha256(source.read_bytes()).hexdigest() != old_sha:
            return False
        shutil.copy2(source, staged)
        staged.chmod(0o755)
        if subprocess.run([str(staged), "--version"], stdin=subprocess.DEVNULL, stdout=log, stderr=log, check=False).returncode != 0:
            return False
        if os_family == "macos" and not run(["codesign", "--verify", "--strict", str(staged)]):
            return False
        os.replace(staged, binary)
        return True
    except Exception:
        return False
    finally:
        try:
            staged.unlink()
        except FileNotFoundError:
            pass

try:
    time.sleep(2)
    if restart(daemon_identity):
        time.sleep(3)
    if service_active(daemon_identity) and service_active(mcp_identity) and mcp_ready():
        print("upgrade: exact daemon restart verified; persistent MCP remained independently supervised")
        safe_to_unlock = True
    else:
        print("upgrade: new daemon failed; restoring prior binary and exact services")
        restored = restore_old_binary()
        mcp_restored = restored and restart(mcp_identity)
        daemon_restored = mcp_restored and restart(daemon_identity)
        if mcp_restored:
            time.sleep(2)
        if daemon_restored:
            time.sleep(3)
        if restored and service_active(mcp_identity) and service_active(daemon_identity) and mcp_ready():
            print("upgrade: rollback verified")
            safe_to_unlock = True
        else:
            print("CRITICAL: daemon upgrade and rollback could not be proven; upgrade lock retained")
finally:
    if safe_to_unlock:
        try:
            os.rmdir(lock_dir)
        except OSError:
            pass
    else:
        try:
            pathlib.Path(lock_dir, "MANUAL_RECOVERY_REQUIRED").write_text("inspect " + log_path + "\n")
        except OSError:
            pass"#;

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn mcp_common_preflight() -> String {
    let resolver = shell_single_quote(MCP_PORT_RESOLVER_PY);
    format!(
        r#"command -v python3 >/dev/null 2>&1 || {{ echo "upgrade: python3 is required for fail-closed MCP config validation" >&2; exit 1; }}; \
         command -v curl >/dev/null 2>&1 || {{ echo "upgrade: curl is required for MCP readiness" >&2; exit 1; }}; \
         MCP_LOCK_DIR="$HOME/.forgefleet/locks/forgefleetd-upgrade.lock"; \
         mkdir -p "$HOME/.forgefleet/locks" || exit 1; \
         if ! mkdir "$MCP_LOCK_DIR" 2>/dev/null; then echo "upgrade: another or interrupted ForgeFleet upgrade owns $MCP_LOCK_DIR; inspect it before retrying" >&2; exit 1; fi; \
         MCP_LOCK_TRANSFERRED=0; \
         cleanup_mcp_upgrade_lock() {{ [ "$MCP_LOCK_TRANSFERRED" -eq 1 ] || rmdir "$MCP_LOCK_DIR" 2>/dev/null || true; }}; \
         trap cleanup_mcp_upgrade_lock 0 HUP INT TERM; \
         MCP_DB_PORT="$("$HOME/.local/bin/ff" secrets get port.mcp 2>/dev/null || true)"; \
         MCP_PORT="$(python3 -c {resolver} "$HOME/.forgefleet/fleet.toml" "$MCP_DB_PORT")" || exit 1; \
         export MCP_PORT MCP_LOCK_DIR; \
         MCP_URL="http://127.0.0.1:${{MCP_PORT}}/mcp"; \
         MCP_HEALTH_URL="${{MCP_URL}}/health"; \
         mcp_initialize_ready() {{ \
           MCP_READY_ATTEMPT=0; \
           while [ "$MCP_READY_ATTEMPT" -lt 30 ]; do \
             MCP_HEALTH_RESPONSE="$(curl --fail --silent --show-error --max-time 2 "$MCP_HEALTH_URL" 2>/dev/null || true)"; \
             if [ "$MCP_HEALTH_RESPONSE" = "ok" ]; then \
               MCP_INIT_RESPONSE="$(curl --fail --silent --show-error --max-time 3 -H 'Content-Type: application/json' \
                 --data '{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-03-26","capabilities":{{}},"clientInfo":{{"name":"forgefleet-upgrade","version":"1"}}}}}}' \
                 "$MCP_URL" 2>/dev/null || true)"; \
               if printf '%s' "$MCP_INIT_RESPONSE" | python3 -c 'import json,sys; payload=json.load(sys.stdin); result=payload.get("result"); raise SystemExit(0 if isinstance(result,dict) and isinstance(result.get("serverInfo"),dict) and not payload.get("error") else 1)' 2>/dev/null; then \
                 unset MCP_HEALTH_RESPONSE MCP_INIT_RESPONSE; return 0; \
               fi; \
             fi; \
             MCP_READY_ATTEMPT=$((MCP_READY_ATTEMPT + 1)); sleep 1; \
           done; \
           unset MCP_HEALTH_RESPONSE MCP_INIT_RESPONSE; return 1; \
         }};"#
    )
}

fn linux_mcp_preflight() -> String {
    let mut script = mcp_common_preflight();
    script.push_str(
        r#" command -v systemctl >/dev/null 2>&1 || { echo "upgrade: systemctl is required for the Linux service transaction" >&2; exit 1; }; \
         export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"; \
         for MCP_REQUIRED_UNIT in forgefleet-mcp.service forgefleetd.service; do \
           [ "$(systemctl --user show -p LoadState --value "$MCP_REQUIRED_UNIT" 2>/dev/null)" = "loaded" ] \
             && systemctl --user is-enabled --quiet "$MCP_REQUIRED_UNIT" \
             && systemctl --user is-active --quiet "$MCP_REQUIRED_UNIT" \
             || { echo "upgrade: required active+enabled $MCP_REQUIRED_UNIT is missing; rerun the current ForgeFleet onboarding before upgrading" >&2; exit 1; }; \
         done; \
         MCP_EXPECTED_BIN="$HOME/.local/bin/forgefleetd"; \
         [ -x "$MCP_EXPECTED_BIN" ] && [ ! -L "$MCP_EXPECTED_BIN" ] || { echo "upgrade: supervised forgefleetd must be a regular executable" >&2; exit 1; }; \
         for MCP_BINARY_SIDE_FILE in "$MCP_EXPECTED_BIN.new" "$MCP_EXPECTED_BIN.prev" "$MCP_EXPECTED_BIN.rollback.new"; do \
           [ ! -L "$MCP_BINARY_SIDE_FILE" ] || { echo "upgrade: refusing symlinked binary transaction file $MCP_BINARY_SIDE_FILE" >&2; exit 1; }; \
         done; \
         MCP_OLD_SHA="$(python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$MCP_EXPECTED_BIN")" || exit 1; \
         export MCP_OLD_SHA; \
         MCP_UNIT_EXEC="$(systemctl --user show -p ExecStart --value forgefleet-mcp.service)" || exit 1; \
         DAEMON_UNIT_EXEC="$(systemctl --user show -p ExecStart --value forgefleetd.service)" || exit 1; \
         case "$MCP_UNIT_EXEC" in \
           *"path=$MCP_EXPECTED_BIN"*"argv[]=$MCP_EXPECTED_BIN mcp --listen 0.0.0.0:$MCP_PORT ;"*|*"path=$MCP_EXPECTED_BIN"*"argv[]=$MCP_EXPECTED_BIN mcp --listen 127.0.0.1:$MCP_PORT ;"*|*"path=$MCP_EXPECTED_BIN"*"argv[]=$MCP_EXPECTED_BIN mcp --listen [::]:$MCP_PORT ;"*) ;; \
           *) echo "upgrade: forgefleet-mcp.service ExecStart does not match the configured persistent HTTP listener" >&2; exit 1 ;; \
         esac; \
         case "$DAEMON_UNIT_EXEC" in \
           *"path=$MCP_EXPECTED_BIN"*"argv[]=$MCP_EXPECTED_BIN start ;"*) ;; \
           *) echo "upgrade: forgefleetd.service ExecStart does not match the exact daemon identity" >&2; exit 1 ;; \
         esac; \
         rollback_mcp_upgrade() { \
           echo "upgrade: restoring prior binary after MCP restart/readiness failure" >&2; \
           { [ "$(python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$MCP_EXPECTED_BIN.prev" 2>/dev/null)" = "$MCP_OLD_SHA" ] \
             && install -m 755 "$MCP_EXPECTED_BIN.prev" "$MCP_EXPECTED_BIN.rollback.new" \
             && "$MCP_EXPECTED_BIN.rollback.new" --version >/dev/null 2>&1 \
             && mv -f "$MCP_EXPECTED_BIN.rollback.new" "$MCP_EXPECTED_BIN"; } \
             || { rm -f "$MCP_EXPECTED_BIN.rollback.new"; echo "CRITICAL: prior ForgeFleet binary could not be restored; upgrade lock retained" >&2; MCP_LOCK_TRANSFERRED=1; return 1; }; \
           systemctl --user restart forgefleet-mcp.service \
             && systemctl --user is-active --quiet forgefleet-mcp.service \
             && mcp_initialize_ready \
             || { echo "CRITICAL: prior MCP service state could not be proven; upgrade lock retained" >&2; MCP_LOCK_TRANSFERRED=1; return 1; }; \
           return 0; \
         };"#,
    );
    script
}

fn macos_mcp_preflight() -> String {
    let validator = shell_single_quote(MCP_PLIST_VALIDATOR_PY);
    let mut script = mcp_common_preflight();
    script.push_str(&format!(
        r#" command -v launchctl >/dev/null 2>&1 && command -v plutil >/dev/null 2>&1 && command -v codesign >/dev/null 2>&1 || {{ echo "upgrade: launchctl, plutil, and codesign are required for the macOS service transaction" >&2; exit 1; }}; \
         MCP_EXPECTED_BIN="$HOME/.local/bin/forgefleetd"; \
         MCP_PLIST="$HOME/Library/LaunchAgents/com.forgefleet.forgefleet-mcp.plist"; \
         DAEMON_PLIST="$HOME/Library/LaunchAgents/com.forgefleet.forgefleetd.plist"; \
         [ -x "$MCP_EXPECTED_BIN" ] && [ ! -L "$MCP_EXPECTED_BIN" ] || {{ echo "upgrade: supervised forgefleetd must be a regular executable" >&2; exit 1; }}; \
         for MCP_BINARY_SIDE_FILE in "$MCP_EXPECTED_BIN.new" "$MCP_EXPECTED_BIN.prev" "$MCP_EXPECTED_BIN.rollback.new"; do \
           [ ! -L "$MCP_BINARY_SIDE_FILE" ] || {{ echo "upgrade: refusing symlinked binary transaction file $MCP_BINARY_SIDE_FILE" >&2; exit 1; }}; \
         done; \
         MCP_OLD_SHA="$(python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$MCP_EXPECTED_BIN")" || exit 1; \
         export MCP_OLD_SHA; \
         python3 -c {validator} "$MCP_PLIST" "$DAEMON_PLIST" "$MCP_EXPECTED_BIN" "$MCP_PORT" || {{ echo "upgrade: rerun the current ForgeFleet onboarding before upgrading this legacy/mixed topology" >&2; exit 1; }}; \
         USER_ID="$(id -u)"; \
         resolve_launch_domain() {{ \
           if launchctl print "gui/${{USER_ID}}/$1" >/dev/null 2>&1; then printf 'gui/%s/%s' "$USER_ID" "$1"; \
           elif launchctl print "user/${{USER_ID}}/$1" >/dev/null 2>&1; then printf 'user/%s/%s' "$USER_ID" "$1"; \
           else return 1; fi; \
         }}; \
         MCP_DOMAIN="$(resolve_launch_domain com.forgefleet.forgefleet-mcp)" \
           && DAEMON_DOMAIN="$(resolve_launch_domain com.forgefleet.forgefleetd)" \
           || {{ echo "upgrade: exact MCP and daemon LaunchAgents must both be registered; rerun onboarding" >&2; exit 1; }}; \
         case "$(launchctl print "$MCP_DOMAIN")" in *"state = running"*) ;; *) echo "upgrade: persistent MCP LaunchAgent is not running" >&2; exit 1 ;; esac; \
         case "$(launchctl print "$DAEMON_DOMAIN")" in *"state = running"*) ;; *) echo "upgrade: ForgeFleet daemon LaunchAgent is not running" >&2; exit 1 ;; esac; \
         rollback_mcp_upgrade() {{ \
           echo "upgrade: restoring prior signed binary after MCP restart/readiness failure" >&2; \
           {{ [ "$(python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$MCP_EXPECTED_BIN.prev" 2>/dev/null)" = "$MCP_OLD_SHA" ] \
             && install -m 755 "$MCP_EXPECTED_BIN.prev" "$MCP_EXPECTED_BIN.rollback.new" \
             && codesign --verify --strict "$MCP_EXPECTED_BIN.rollback.new" \
             && "$MCP_EXPECTED_BIN.rollback.new" --version >/dev/null 2>&1 \
             && mv -f "$MCP_EXPECTED_BIN.rollback.new" "$MCP_EXPECTED_BIN"; }} \
             || {{ rm -f "$MCP_EXPECTED_BIN.rollback.new"; echo "CRITICAL: prior signed binary could not be restored; upgrade lock retained" >&2; MCP_LOCK_TRANSFERRED=1; return 1; }}; \
           launchctl kickstart -k "$MCP_DOMAIN" \
             && mcp_initialize_ready \
             || {{ echo "CRITICAL: prior MCP LaunchAgent state could not be proven; upgrade lock retained" >&2; MCP_LOCK_TRANSFERRED=1; return 1; }}; \
           return 0; \
         }};"#
    ));
    script
}

fn detached_daemon_restart_cmd(
    os_family: &str,
    daemon_identity: &str,
    mcp_identity: &str,
) -> String {
    let restarter = shell_single_quote(DETACHED_DAEMON_RESTART_PY);
    format!(
        r#"MCP_LOCK_TRANSFERRED=1; \
         MCP_RESTART_PID="$(python3 -c {restarter} {os_family} "{daemon_identity}" "{mcp_identity}" "$MCP_EXPECTED_BIN" "$MCP_OLD_SHA" "$MCP_PORT" "$MCP_LOCK_DIR" "$HOME/.forgefleet/logs/forgefleetd-upgrade-restart.log")" \
           || {{ MCP_LOCK_TRANSFERRED=0; rollback_mcp_upgrade; exit 1; }}; \
         case "$MCP_RESTART_PID" in ''|*[!0-9]*) MCP_LOCK_TRANSFERRED=0; rollback_mcp_upgrade; echo "upgrade: invalid detached restart receipt" >&2; exit 1 ;; esac; \
         echo "build+install+MCP initialize OK; exact daemon restart transaction detached as pid $MCP_RESTART_PID""#
    )
}

fn forgefleetd_linux_playbook(build_suffix: &str) -> String {
    let preflight = linux_mcp_preflight();
    let restart =
        detached_daemon_restart_cmd("linux", "forgefleetd.service", "forgefleet-mcp.service");
    format!(
        r#"{preflight} \
         . "$HOME/.cargo/env" 2>/dev/null || true; \
         {sync} && \
         {web_build} && \
         cargo build --bin forgefleetd --release{build_suffix} && {install}; \
         if ! systemctl --user restart forgefleet-mcp.service; then \
           rollback_mcp_upgrade; exit 1; \
         fi; \
         if ! systemctl --user is-active --quiet forgefleet-mcp.service || ! mcp_initialize_ready; then \
           rollback_mcp_upgrade; exit 1; \
         fi; \
         {restart}"#,
        sync = GIT_SYNC_FORGE_FLEET,
        web_build = WEB_BUILD_STEP,
        install = atomic_install_cmd("forgefleetd", "$HOME/.local/bin/forgefleetd", false),
    )
}

fn forgefleetd_macos_playbook() -> String {
    let preflight = macos_mcp_preflight();
    let restart = detached_daemon_restart_cmd("macos", "$DAEMON_DOMAIN", "$MCP_DOMAIN");
    format!(
        r#"{preflight} \
         . "$HOME/.cargo/env" 2>/dev/null || true; \
         {sync} && \
         {web_build} && \
         cargo build --bin forgefleetd --release && {install}; \
         if ! launchctl kickstart -k "$MCP_DOMAIN"; then \
           rollback_mcp_upgrade; exit 1; \
         fi; \
         if ! mcp_initialize_ready; then \
           rollback_mcp_upgrade; exit 1; \
         fi; \
         {restart}"#,
        sync = GIT_SYNC_FORGE_FLEET,
        web_build = WEB_BUILD_STEP,
        install = atomic_install_cmd("forgefleetd", "$HOME/.local/bin/forgefleetd", true),
    )
}

fn playbook_exact(tool: &str, os_family: &str) -> Option<String> {
    match (tool, os_family) {
        ("gh", "linux") => {
            Some("sudo apt-get update && sudo apt-get install --only-upgrade -y gh".into())
        }
        ("gh", "macos") => Some("brew upgrade gh".into()),
        ("op", "linux") => Some(
            "sudo apt-get update && sudo apt-get install --only-upgrade -y 1password-cli".into(),
        ),
        ("op", "macos") => Some("brew upgrade --cask 1password-cli".into()),
        // Claude Code is a NATIVE install (~/.local/share/claude/versions/<v>
        // with a ~/.local/bin/claude symlink it manages itself) — NOT npm/brew,
        // so the canonical upgrade is its own self-updater `claude update`
        // ("check for updates and install if available", which fetches the
        // latest native build and repoints the symlink). Identical on every OS.
        // Without this arm every `tool=claude` upgrade task failed
        // "no playbook for tool=claude" (108+ deferred-task failures/24h). The
        // PATH export makes the symlink resolvable under the daemon's non-login
        // /bin/sh.
        ("claude", _) => Some("export PATH=\"$HOME/.local/bin:$PATH\"; claude update".into()),
        ("mlx_lm", _) => Some("pip install -U mlx-lm".into()),
        ("vllm", _) => Some("pip install -U vllm".into()),
        ("llama.cpp", _) => {
            Some("cd ~/llama.cpp && git pull && cmake --build build --config Release -j".into())
        }
        // Cargo binaries (ff CLI + forgefleetd daemon). Playbooks source
        // ~/.cargo/env because they execute under `sh` (Ubuntu /bin/sh =
        // dash) without the operator's interactive PATH — the rustup-managed
        // cargo at $HOME/.cargo/bin would otherwise fall back to PATH and
        // fail with `cargo: not found`. Tracking down that one-line error
        // cost a fleet-wide upgrade attempt 2026-05-16. Use `. <file>`
        // (POSIX `source`) so dash + bash both load it.
        ("ff_git" | "ff", "macos") => Some(format!(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             {sync} && \
             cargo build -p ff-terminal --release && {install}",
            sync = GIT_SYNC_FORGE_FLEET,
            install = atomic_install_cmd("ff", "$HOME/.local/bin/ff", true),
        )),
        ("ff_git" | "ff", "linux") => Some(format!(
            ". \"$HOME/.cargo/env\" 2>/dev/null || true; \
             {sync} && \
             cargo build -p ff-terminal --release && {install}",
            sync = GIT_SYNC_FORGE_FLEET,
            install = atomic_install_cmd("ff", "$HOME/.local/bin/ff", false),
        )),
        // The shared binary backs two independently supervised services.  A
        // daemon-only legacy topology is not upgradeable: losing the worker
        // must never also strand every MCP client.  Preflight therefore proves
        // exact supervisor identity and configured port before touching disk.
        // The MCP service is upgraded and accepts a fresh initialize request
        // first.  Only then does a new-session child restart the daemon; that
        // child owns the recovery lock and restores the prior binary/services
        // if the new daemon fails.  No process-name matching is permitted.
        ("forgefleetd_git" | "forgefleetd", "macos") => Some(forgefleetd_macos_playbook()),
        ("forgefleetd_git" | "forgefleetd", "linux") => Some(forgefleetd_linux_playbook("")),
        // DGX Sparks: aarch64 + 4 cores. `-j 2` prevents LLVM OOM while
        // retaining the same fail-closed service transaction as Linux.
        ("forgefleetd_git" | "forgefleetd", "linux-dgx") => {
            Some(forgefleetd_linux_playbook(" -j 2"))
        }
        ("os", "linux") => Some("sudo apt-get update && sudo apt-get -y upgrade".into()),
        ("os", "macos") => Some("softwareupdate -i -a".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Output};

    fn resolve_test_port(config: Option<&str>, db_port: &str) -> Output {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fleet.toml");
        if let Some(contents) = config {
            std::fs::write(&path, contents).unwrap();
        }
        Command::new("python3")
            .args(["-c", MCP_PORT_RESOLVER_PY])
            .arg(&path)
            .arg(db_port)
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn spawn_mock_mcp() -> (
        u16,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{Read, Write};
        use std::sync::atomic::Ordering;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        let mut request = [0_u8; 16_384];
                        let count = stream.read(&mut request).unwrap_or(0);
                        let request = String::from_utf8_lossy(&request[..count]);
                        let body = if request.starts_with("GET /mcp/health ") {
                            "ok"
                        } else {
                            r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"mock-mcp","version":"1"}}}"#
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        stream.write_all(response.as_bytes()).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(error) => panic!("mock MCP accept failed: {error}"),
                }
            }
        });
        (port, stop, handle)
    }

    #[test]
    fn sub_family_falls_back_to_base() {
        // The regression: linux-ubuntu / linux-dgx etc. must resolve the
        // generic `linux` playbook when no specialised arm exists.
        for fam in ["linux-ubuntu", "linux", "linux-fedora"] {
            let p = playbook_for("forgefleetd_git", fam)
                .unwrap_or_else(|| panic!("no playbook for {fam}"));
            assert!(p.contains("cargo build --bin forgefleetd"), "fam={fam}");
        }
    }

    #[test]
    fn specialised_arm_wins_over_base() {
        // linux-dgx has its own `-j 2` arm — exact match must take priority.
        let dgx = playbook_for("forgefleetd_git", "linux-dgx").unwrap();
        assert!(dgx.contains("-j 2"), "dgx arm should be the -j 2 variant");
    }

    #[test]
    fn macos_sub_family_falls_back() {
        let p = playbook_for("forgefleetd_git", "macos-15").unwrap();
        assert!(p.contains("launchctl kickstart"));
    }

    #[test]
    fn claude_resolves_to_self_updater_on_every_os() {
        // The native Claude Code install self-updates via `claude update`; the
        // wildcard arm must resolve across linux/macos sub-families (was the
        // "no playbook for tool=claude" failure source).
        for fam in ["linux-ubuntu", "linux", "macos", "macos-15", "linux-dgx"] {
            let p = playbook_for("claude", fam)
                .unwrap_or_else(|| panic!("no claude playbook for {fam}"));
            assert!(p.contains("claude update"), "fam={fam}");
        }
    }

    #[test]
    fn unknown_os_is_none() {
        assert!(playbook_for("forgefleetd_git", "plan9").is_none());
    }

    #[test]
    fn forge_fleet_upgrades_reset_hard_never_pull() {
        // The 748/24h `fatal: Cannot fast-forward to multiple branches` failures
        // (marcus+logan): `git pull --ff-only` can't recover a diverged checkout.
        // Every forge-fleet build playbook must sync via `git reset --hard
        // origin/main` (divergence-proof, same as deploy) and NEVER `git pull`.
        for tool in ["ff_git", "forgefleetd_git"] {
            for fam in ["macos", "macos-15", "linux", "linux-ubuntu", "linux-dgx"] {
                let p = playbook_for(tool, fam)
                    .unwrap_or_else(|| panic!("no playbook for {tool}/{fam}"));
                assert!(
                    p.contains("git reset --hard origin/main"),
                    "{tool}/{fam} must hard-reset to origin/main"
                );
                assert!(
                    p.contains("git fetch origin"),
                    "{tool}/{fam} must fetch before reset"
                );
                assert!(
                    !p.contains("git pull"),
                    "{tool}/{fam} must NOT use the fragile git pull"
                );
            }
        }
    }

    #[test]
    fn atomic_install_uses_temp_validate_then_rename() {
        // The ace 2026-06-14 brick: a disk-full `install` straight into
        // ~/.local/bin/ff left a 304-byte garbage binary in PATH. The install
        // must go to a temp, prove it runs, then atomically rename — and on
        // failure remove the temp + exit non-zero so PATH keeps the old binary.
        let mac = atomic_install_cmd("ff", "$HOME/.local/bin/ff", true);
        assert!(mac.contains("install -m 755 target/release/ff \"$HOME/.local/bin/ff.new\""));
        assert!(mac.contains("codesign --force --sign - \"$HOME/.local/bin/ff.new\""));
        assert!(mac.contains("\"$HOME/.local/bin/ff.new\" --version"));
        assert!(mac.contains("mv -f \"$HOME/.local/bin/ff.new\" \"$HOME/.local/bin/ff\""));
        assert!(mac.contains("rm -f \"$HOME/.local/bin/ff.new\""));
        assert!(mac.contains("exit 1"));

        // Linux build has no code-signing step.
        let lin = atomic_install_cmd("forgefleetd", "$HOME/.local/bin/forgefleetd", false);
        assert!(!lin.contains("codesign"));
        assert!(lin.contains("\"$HOME/.local/bin/forgefleetd.new\" --version"));
        assert!(lin.contains("mv -f \"$HOME/.local/bin/forgefleetd.new\""));
    }

    #[test]
    fn cargo_binary_playbooks_install_atomically() {
        // Every cargo-binary upgrade arm must validate-then-rename (never write
        // straight into PATH) so an interrupted/disk-full copy can't brick the
        // host's CLI or daemon binary.
        for (tool, fam) in [
            ("ff_git", "macos"),
            ("ff_git", "linux"),
            ("forgefleetd_git", "macos"),
            ("forgefleetd_git", "linux"),
            ("forgefleetd_git", "linux-dgx"),
        ] {
            let p = playbook_for(tool, fam).unwrap_or_else(|| panic!("no playbook {tool}/{fam}"));
            assert!(
                p.contains(".new\""),
                "{tool}/{fam}: not installing to a temp"
            );
            assert!(
                p.contains(".new\" --version"),
                "{tool}/{fam}: not validated"
            );
            assert!(p.contains("mv -f"), "{tool}/{fam}: not atomically renamed");
            // Must NOT write the final binary directly (the old poisoning path).
            assert!(
                !p.contains("install -m 755 target/release/ff \"$HOME/.local/bin/ff\"")
                    && !p.contains(
                        "install -m 755 target/release/forgefleetd \"$HOME/.local/bin/forgefleetd\""
                    ),
                "{tool}/{fam}: still installs directly into PATH"
            );
        }
    }

    #[test]
    fn mcp_port_authority_accepts_db_only_and_consistent_local_sources() {
        let db_only = resolve_test_port(None, "50001");
        assert!(db_only.status.success());
        assert_eq!(String::from_utf8(db_only.stdout).unwrap().trim(), "50001");

        let all_sources = resolve_test_port(
            Some(
                r#"[ports]
forgefleet = 50001
[mcp.forgefleet]
port = 50001
endpoint = "http://127.0.0.1:50001/mcp"
"#,
            ),
            "50001",
        );
        assert!(all_sources.status.success());
        assert_eq!(
            String::from_utf8(all_sources.stdout).unwrap().trim(),
            "50001"
        );
    }

    #[test]
    fn mcp_port_authority_rejects_missing_conflicting_and_invalid_sources() {
        let missing = resolve_test_port(Some("[general]\nname = \"ForgeFleet\"\n"), "");
        assert!(!missing.status.success());
        assert!(String::from_utf8_lossy(&missing.stderr).contains("is missing"));

        let conflict = resolve_test_port(
            Some(
                r#"[ports]
forgefleet = 50001
[mcp.forgefleet]
endpoint = "http://127.0.0.1:50002/mcp"
"#,
            ),
            "50001",
        );
        assert!(!conflict.status.success());
        assert!(String::from_utf8_lossy(&conflict.stderr).contains("sources disagree"));

        let invalid = resolve_test_port(
            Some("[mcp.forgefleet]\nendpoint = \"http://127.0.0.1:50001/not-mcp\"\n"),
            "",
        );
        assert!(!invalid.status.success());
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid MCP endpoint path"));

        let out_of_range = resolve_test_port(None, "70000");
        assert!(!out_of_range.status.success());
        assert!(String::from_utf8_lossy(&out_of_range.stderr).contains("is invalid"));
    }

    #[test]
    fn macos_plist_validator_requires_exact_two_service_topology_and_port() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("forgefleetd");
        let mcp = temp.path().join("mcp.plist");
        let daemon = temp.path().join("daemon.plist");
        let binary_text = binary.to_string_lossy();
        let mcp_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>Label</key><string>com.forgefleet.forgefleet-mcp</string>
<key>ProgramArguments</key><array><string>{binary_text}</string><string>mcp</string><string>--listen</string><string>0.0.0.0:50001</string></array>
</dict></plist>"#
        );
        let daemon_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>Label</key><string>com.forgefleet.forgefleetd</string>
<key>ProgramArguments</key><array><string>{binary_text}</string><string>--node-name</string><string>ace</string><string>start</string></array>
</dict></plist>"#
        );
        std::fs::write(&mcp, mcp_xml).unwrap();
        std::fs::write(&daemon, &daemon_xml).unwrap();

        let valid = Command::new("python3")
            .args(["-c", MCP_PLIST_VALIDATOR_PY])
            .arg(&mcp)
            .arg(&daemon)
            .arg(&binary)
            .arg("50001")
            .output()
            .unwrap();
        assert!(
            valid.status.success(),
            "{}",
            String::from_utf8_lossy(&valid.stderr)
        );

        // Keep upgrade compatibility with the older, still-exact flag spelling.
        std::fs::write(&daemon, daemon_xml.replace("--node-name", "--worker-name")).unwrap();
        let legacy_named = Command::new("python3")
            .args(["-c", MCP_PLIST_VALIDATOR_PY])
            .arg(&mcp)
            .arg(&daemon)
            .arg(&binary)
            .arg("50001")
            .output()
            .unwrap();
        assert!(
            legacy_named.status.success(),
            "{}",
            String::from_utf8_lossy(&legacy_named.stderr)
        );

        let wrong_port = Command::new("python3")
            .args(["-c", MCP_PLIST_VALIDATOR_PY])
            .arg(&mcp)
            .arg(&daemon)
            .arg(&binary)
            .arg("50002")
            .output()
            .unwrap();
        assert!(!wrong_port.status.success());
        assert!(String::from_utf8_lossy(&wrong_port.stderr).contains("configured persistent HTTP"));
    }

    #[cfg(unix)]
    #[test]
    fn linux_mcp_failure_restores_hash_bound_prior_binary_with_mock_supervisor() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let fake_bin = temp.path().join("fake-bin");
        let local_bin = home.join(".local/bin");
        std::fs::create_dir_all(&fake_bin).unwrap();
        std::fs::create_dir_all(&local_bin).unwrap();
        let daemon = local_bin.join("forgefleetd");
        write_executable(
            &daemon,
            "#!/bin/sh\n# OLD\n[ \"$1\" = \"--version\" ] && exit 0\nexit 0\n",
        );
        write_executable(
            &local_bin.join("ff"),
            "#!/bin/sh\n[ \"$1 $2\" = \"secrets get\" ] && { printf '50001\\n'; exit 0; }\nexit 1\n",
        );
        write_executable(
            &fake_bin.join("systemctl"),
            r#"#!/bin/sh
case "$*" in
  *"show -p LoadState --value"*) printf 'loaded\n' ;;
  *"show -p ExecStart --value forgefleet-mcp.service"*) printf '{ path=%s/.local/bin/forgefleetd ; argv[]=%s/.local/bin/forgefleetd mcp --listen 0.0.0.0:50001 ; ignore_errors=no ; }\n' "$HOME" "$HOME" ;;
  *"show -p ExecStart --value forgefleetd.service"*) printf '{ path=%s/.local/bin/forgefleetd ; argv[]=%s/.local/bin/forgefleetd start ; ignore_errors=no ; }\n' "$HOME" "$HOME" ;;
  *) exit 0 ;;
esac
"#,
        );
        write_executable(
            &fake_bin.join("curl"),
            r#"#!/bin/sh
case "$*" in
  *"/mcp/health"*) printf 'ok' ;;
  *) printf '{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"mock","version":"1"}}}' ;;
esac
"#,
        );

        let script = format!(
            r#"{}
cp "$MCP_EXPECTED_BIN" "$MCP_EXPECTED_BIN.prev" || exit 1
printf '%s\n' '#!/bin/sh' '# NEW' '[ "$1" = "--version" ] && exit 0' 'exit 0' > "$MCP_EXPECTED_BIN"
chmod 755 "$MCP_EXPECTED_BIN"
rollback_mcp_upgrade
"#,
            linux_mcp_preflight()
        );
        let path = format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new("sh")
            .args(["-c", &script])
            .env("HOME", &home)
            .env("PATH", path)
            .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let restored = std::fs::read_to_string(&daemon).unwrap();
        assert!(restored.contains("# OLD"));
        assert!(!restored.contains("# NEW"));
        assert!(
            !home
                .join(".forgefleet/locks/forgefleetd-upgrade.lock")
                .exists()
        );
    }

    #[cfg(unix)]
    fn exercise_detached_restart(fail_new_daemon: bool) -> (String, String) {
        use sha2::{Digest, Sha256};
        use std::sync::atomic::Ordering;

        let temp = tempfile::tempdir().unwrap();
        let fake_bin = temp.path().join("fake-bin");
        std::fs::create_dir_all(&fake_bin).unwrap();
        let daemon = temp.path().join("forgefleetd");
        let prior = "#!/bin/sh\n# OLD\n[ \"$1\" = \"--version\" ] && exit 0\nexit 0\n";
        let current = "#!/bin/sh\n# NEW\n[ \"$1\" = \"--version\" ] && exit 0\nexit 0\n";
        write_executable(&daemon, current);
        write_executable(&temp.path().join("forgefleetd.prev"), prior);
        write_executable(
            &fake_bin.join("systemctl"),
            r#"#!/bin/sh
last=''
for value in "$@"; do last="$value"; done
if [ "${TEST_FAIL_NEW:-0}" = "1" ] && [ "$last" = "forgefleetd.service" ] && grep -q '# NEW' "$TEST_BINARY"; then
  case "$*" in *"is-active"*) exit 1 ;; esac
fi
exit 0
"#,
        );
        let old_sha = format!("{:x}", Sha256::digest(prior.as_bytes()));
        let lock = temp.path().join("upgrade.lock");
        std::fs::create_dir(&lock).unwrap();
        let log = temp.path().join("restart.log");
        let (port, stop, server) = spawn_mock_mcp();
        let path = format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new("python3")
            .args(["-c", DETACHED_DAEMON_RESTART_PY, "linux"])
            .arg("forgefleetd.service")
            .arg("forgefleet-mcp.service")
            .arg(&daemon)
            .arg(&old_sha)
            .arg(port.to_string())
            .arg(&lock)
            .arg(&log)
            .env("PATH", path)
            .env("TEST_BINARY", &daemon)
            .env("TEST_FAIL_NEW", if fail_new_daemon { "1" } else { "0" })
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<u32>()
                .is_ok(),
            "missing detached PID receipt"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while lock.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert!(
            !lock.exists(),
            "detached transaction did not reach a verified state: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        (
            std::fs::read_to_string(&log).unwrap(),
            std::fs::read_to_string(&daemon).unwrap(),
        )
    }

    #[cfg(unix)]
    #[test]
    fn detached_restart_survives_parent_and_reprobes_persistent_mcp() {
        let (log, binary) = exercise_detached_restart(false);
        assert!(log.contains("exact daemon restart verified"));
        assert!(log.contains("persistent MCP remained independently supervised"));
        assert!(binary.contains("# NEW"));
    }

    #[cfg(unix)]
    #[test]
    fn detached_daemon_failure_restores_old_binary_and_both_services() {
        let (log, binary) = exercise_detached_restart(true);
        assert!(log.contains("new daemon failed; restoring prior binary"));
        assert!(log.contains("rollback verified"));
        assert!(binary.contains("# OLD"));
        assert!(!binary.contains("# NEW"));
    }

    #[test]
    fn daemon_upgrade_preflights_then_proves_mcp_before_detached_restart() {
        for fam in ["linux", "linux-dgx", "macos"] {
            let playbook = playbook_for("forgefleetd_git", fam)
                .unwrap_or_else(|| panic!("no playbook for {fam}"));
            let authority = playbook.find("MCP_DB_PORT=").unwrap();
            let topology = if fam == "macos" {
                playbook.find("MCP_PLIST=").unwrap()
            } else {
                playbook.find("MCP_REQUIRED_UNIT").unwrap()
            };
            let source_sync = playbook.find("git fetch origin").unwrap();
            let install = playbook
                .find("mv -f \"$HOME/.local/bin/forgefleetd.new\"")
                .unwrap();
            let mcp_restart = if fam == "macos" {
                playbook[install..]
                    .find("launchctl kickstart -k \"$MCP_DOMAIN\"")
                    .map(|offset| offset + install)
                    .unwrap()
            } else {
                playbook[install..]
                    .find("systemctl --user restart forgefleet-mcp.service")
                    .map(|offset| offset + install)
                    .unwrap()
            };
            let readiness_after_restart = playbook[mcp_restart..]
                .find("mcp_initialize_ready")
                .map(|offset| offset + mcp_restart)
                .unwrap();
            let detached = playbook.find("DETACHED_RESTART_PID").unwrap_or_else(|| {
                playbook
                    .find("MCP_RESTART_PID")
                    .expect("detached restart receipt")
            });
            assert!(
                authority < topology
                    && topology < source_sync
                    && source_sync < install
                    && install < mcp_restart
                    && mcp_restart < readiness_after_restart
                    && readiness_after_restart < detached,
                "fam={fam}: unsafe lifecycle ordering"
            );
            assert!(playbook.contains("MCP_EXPECTED_BIN.prev"), "fam={fam}");
            assert!(playbook.contains("restore_old_binary"), "fam={fam}");
            assert!(playbook.contains("rollback verified"), "fam={fam}");
            assert!(playbook.contains("rerun") && playbook.contains("onboarding"));
            assert!(!playbook.contains("127.0.0.1:50001"), "fam={fam}");
            assert!(!playbook.contains("0.0.0.0:50001"), "fam={fam}");
            for forbidden in ["pkill", "pgrep", "killall", "disown", "nohup"] {
                assert!(
                    !playbook.contains(forbidden),
                    "fam={fam}: contains {forbidden}"
                );
            }
            let syntax = Command::new("sh")
                .args(["-n", "-c", &playbook])
                .output()
                .unwrap();
            assert!(
                syntax.status.success(),
                "fam={fam}: {}",
                String::from_utf8_lossy(&syntax.stderr)
            );
        }
    }

    #[test]
    fn detached_restart_program_is_syntax_valid_and_new_session_based() {
        let compile = Command::new("python3")
            .args([
                "-c",
                "import sys; compile(sys.argv[1], 'detached_restart', 'exec')",
                DETACHED_DAEMON_RESTART_PY,
            ])
            .output()
            .unwrap();
        assert!(
            compile.status.success(),
            "{}",
            String::from_utf8_lossy(&compile.stderr)
        );
        assert!(DETACHED_DAEMON_RESTART_PY.contains("os.setsid()"));
        assert!(DETACHED_DAEMON_RESTART_PY.contains("service_active(mcp_identity)"));
        assert!(DETACHED_DAEMON_RESTART_PY.contains("mcp_ready()"));
        assert!(DETACHED_DAEMON_RESTART_PY.contains("MANUAL_RECOVERY_REQUIRED"));
    }

    #[test]
    fn base_family_maps_sub_families_to_their_base() {
        // Shared with auto_upgrade's playbook-key fallback. A regression here
        // (e.g. dropping the `starts_with` so `linux-ubuntu` no longer maps to
        // `linux`) silently skips every Linux target — the 2026-04-30 outage.
        assert_eq!(base_family("linux"), Some("linux"));
        assert_eq!(base_family("linux-ubuntu"), Some("linux"));
        assert_eq!(base_family("linux-dgx"), Some("linux"));
        assert_eq!(base_family("linux-fedora"), Some("linux"));
        assert_eq!(base_family("macos"), Some("macos"));
        assert_eq!(base_family("macos-26"), Some("macos"));
        assert_eq!(base_family("windows"), Some("windows"));
        assert_eq!(base_family("windows-11"), Some("windows"));
        assert_eq!(base_family("plan9"), None);
        assert_eq!(base_family(""), None);
    }
}
