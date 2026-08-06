# macOS enrollment operator notes

Hard-won notes from the vinny re-onboarding (2026-08-03/06). Read this once
before onboarding a Mac; it saves hours.

## Before you run the bootstrap

1. **Network**: the Mac must be on the fleet LAN (192.168.5.x). **Turn Wi-Fi
   off** — if Wi-Fi is on and lands on a different subnet (192.168.4.x), the
   Mac downloads nothing and the bootstrap's preflight now tells you this
   immediately.
2. **Remote Login**: System Settings → General → Sharing → **Remote Login → On**.
   Without it, no fleet SSH/deploys to this machine, and everything must be
   driven by hand on the console. (New bootstraps attempt to enable it
   automatically; the step reports `remote_login` failed with this exact
   instruction when macOS refuses.)
3. **Firewall**: System Settings → Network → **Firewall → Off**, or add an
   allow rule for `forgefleetd`. A fresh Mac blocks inbound to unsigned
   binaries by default, which leaves the gateway answering localhost-only.

## Running the bootstrap

- Always use the **download-first** form (`ff onboard show --name <n>` prints
  it). Never `curl | bash` — a truncated stream used to kill the script
  silently mid-run.
- Run it in **bash**, not zsh: the script targets bash, and if you ever paste
  commands by hand, remember zsh does NOT treat `#` lines as comments and
  mangles heredocs. Type `bash` first, then paste.
- The script is **idempotent** — Ctrl+C and re-run freely; completed phases
  skip fast.
- If it stops, look for GUI dialogs (Command Line Tools, keychain) — a fresh
  Mac throws them on first use of dev tools.

## Desktop apps

The bootstrap downloads Claude / Kimi / ChatGPT installers into
`~/Downloads/fleet-desktop-apps/` — **install them yourself** from there
(operator preference; no silent GUI installs). Codex's desktop mode lives in
the ChatGPT app; there is no standalone Codex desktop anymore.

## MCP wiring

- CLI sessions (claude/codex/kimi) read MCP config at startup — **restart the
  CLI** after bootstrap before expecting forgefleet tools.
- Desktop apps (Claude Desktop, Kimi Desktop) talk to the MCP listener on
  `127.0.0.1:50001`. If they show "disconnected": check
  `launchctl print gui/$(id -u)/com.forgefleet.forgefleet-mcp` and
  `lsof -nP -iTCP:50001 -sTCP:LISTEN`.
- `ff mcp status` shows per-client wiring; `ff converge` re-applies all of it.

## If the daemon won't start

`~/.local/bin/forgefleetd start` in a terminal prints the real error (config
preflight, DB connect, ports). The launchd plist hides it. And remember:
`launchctl kickstart -k` does NOT apply plist edits — `bootout` then
`bootstrap` when the plist changes.
