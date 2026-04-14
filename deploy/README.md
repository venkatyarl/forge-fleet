# ForgeFleet deployment — service files

This directory contains service definitions to run `ff daemon` as a
long-running background service on each fleet node.

`ff daemon` bundles three periodic tasks:

| Task                  | Default interval | Purpose                                           |
|-----------------------|------------------|---------------------------------------------------|
| Deferred-task worker  | 15 s             | Claim + execute deferred shell/http tasks         |
| Disk usage sampler    | 300 s            | Snapshot free / used / models-dir bytes to Postgres |
| Deployment reconciler | 60 s             | Sync `fleet_model_deployments` with real processes  |

Exactly **one** fleet node (typically the leader, `taylor`) should run with
`--scheduler`. Others run workers only.

---

## macOS — launchd (Taylor, Ace, James)

```bash
# 1. Copy the template and update USER paths if your username isn't `venkat`:
cp deploy/launchd/com.forgefleet.daemon.plist ~/Library/LaunchAgents/

# 2. Load and start
launchctl load ~/Library/LaunchAgents/com.forgefleet.daemon.plist
launchctl start com.forgefleet.daemon

# 3. Logs
tail -f ~/.forgefleet/logs/daemon.out.log
tail -f ~/.forgefleet/logs/daemon.err.log

# 4. Stop
launchctl stop com.forgefleet.daemon
launchctl unload ~/Library/LaunchAgents/com.forgefleet.daemon.plist
```

The plist is configured with `--scheduler`. On worker nodes, remove the
`<string>--scheduler</string>` line before installing.

---

## Linux — systemd (Marcus, Sophie, Priya, Duncan, Lily, Logan, Veronica, Aura)

The unit uses the `@` instance template so you can specialize per user:

```bash
# On each Linux node, substitute USERNAME for the box's login user (marcus, sophie, ...):
sudo cp deploy/systemd/forgefleet-daemon.service /etc/systemd/system/forgefleet-daemon@.service
sudo systemctl daemon-reload

# Enable + start as that user (e.g. on marcus as `marcus`):
sudo systemctl enable  forgefleet-daemon@marcus.service
sudo systemctl start   forgefleet-daemon@marcus.service

# Status / logs
sudo systemctl status forgefleet-daemon@marcus.service
journalctl -u forgefleet-daemon@marcus.service -f
```

The provided unit does NOT include `--scheduler` — it's worker-only.
Leader (Taylor) on Mac runs the scheduler via launchd.

---

## Verifying the daemon is healthy

After install, from any fleet member:

```bash
# Check that deferred tasks are being promoted and claimed.
ff defer list

# Check disk-usage snapshots are landing.
ff model disk

# Check that running processes match deployment rows.
ff model deployments
ff model ps
```

All three should show sensible fresh data within a few minutes of startup.

---

## Stopping / upgrading

After installing a new `ff` binary:

```bash
# macOS
launchctl stop com.forgefleet.daemon   # picks up new binary on next auto-restart

# Linux
sudo systemctl restart forgefleet-daemon@${USER}.service
```

The `KeepAlive`/`Restart=on-failure` policies ensure the daemon comes back
if it crashes.
