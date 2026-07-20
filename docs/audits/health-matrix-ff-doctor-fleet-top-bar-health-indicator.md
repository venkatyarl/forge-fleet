# Audit: fleet health matrix and top-bar indicator

Date: 2026-07-20  
Work item: Health matrix: `ff doctor --fleet` (9 layers) + top-bar health indicator

## Outcome

The requested matrix is not exposed today. Two prerequisite fixes are already present: `ff fleet health` reports computer reachability separately from daemon membership, and each daemon checks the freshness of its own expected local backups against twice the configured interval. The remaining work should compose existing signals into one shared per-node snapshot, render it from `ff doctor --fleet`, and feed the same snapshot to the TUI header. No database migration is required for the proposed implementation; the live schema already contains the necessary durable fields.

## Current command and UI behavior

- `ff doctor` accepts only `--json` and `--strict` (`crates/ff-terminal/src/main.rs`). Its checks in `doctor_cmd.rs` are aggregate operational checks, not a per-computer matrix.
- `ff fleet health` (`fleet_cmd.rs::handle_fleet_health`) independently probes the SSH TCP port and derives daemon membership from Pulse. Its text and JSON outputs already expose `computer_reachable` and `daemon_joined` as distinct values, satisfying the intent of work item `c1953337`.
- The TUI refreshes daemon and model `/health` endpoints every 30 seconds (`main.rs::kick_fleet_health_refresh`), but `render.rs::render_header` displays only turn/project/model/web information. Node dots in the sidebar represent daemon HTTP response only and are not fleet health.
- `doctor_cmd::handle_doctor` directly executes its own SQL and local checks. Cortex reports no test that covers the handler, although its status helpers and renderer have unit tests.

## Nine-layer inventory

| Layer | Existing source to reuse | What is missing from the matrix |
|---|---|---|
| 1. Reachability | `fleet_cmd.rs::computer_reachable` probes each computer's configured SSH port | Move/share the probe outside `fleet_cmd`; retain a bounded timeout and represent timeout/refusal as down, not daemon-absent |
| 2. Daemon joined | Pulse `beats_by_name`, SDOWN/ODOWN state, and `computers.last_seen_at`; gateway DTOs also expose `daemon_joined: Option<bool>` | Preserve three states (`joined`, `absent`, `unknown`) so a Pulse/DB outage is not mislabeled as every daemon being absent |
| 3. Services | Daemon `/health`, model endpoints, `port_registry`, Pulse Docker/service observations | Define the required service set by node role and report `pass/warn/fail/unknown`; registry configuration alone must not count as a successful live probe |
| 4. Auth keys | `computer_trust.last_probe_at`, `last_probe_status`, `revoked_at`; `ssh_key_manager` owns fan-out/rotation | Aggregate directed trust edges for each node and flag missing, revoked, failed, or stale probes without exposing key material |
| 5. LLM probe | `deployment_reconciler` calls `probe_health_public`; `fleet_model_deployments.health_status/last_health_at` | Aggregate active deployments per node; distinguish no deployment (not applicable) from an expected deployment that is unhealthy or stale |
| 6. LLM longevity/auto-restart | Deployment rows contain `started_at`, `pid`, desired state, request count; runtime installs systemd restart behavior; reconciler repairs process/row drift | There is no durable restart counter or last-exit reason. Initially infer only `stable` versus `recently started`; a later telemetry work item should add restart count/last restart if operators need crash-loop diagnosis |
| 7. Disk/quota | `fleet_disk_usage` plus worker quota; the existing doctor already computes aggregate over/near counts | Return latest sample age and per-node percent/quota verdict; missing or stale samples must be `unknown`, not pass |
| 8. Replica lag | `database_replicas.lag_bytes`, `last_sync_at`, role/status; handoff has `MAX_SAFE_LAG_BYTES` | Reuse one shared threshold policy and include sample staleness. Primary/non-replica nodes are not applicable, not green replicas |
| 9. Backup freshness | `BackupManager::check_local_backup_freshness` runs on every daemon and evaluates expected holders at `2 * max(interval_secs, 60)`; `backups`, `fleet_backup_config`, and durable alerts exist | Surface the per-node result in the snapshot. Do not fall back to the older synthetic probe, which checks only the leader and uses fixed 24h/48h thresholds |

## Recommended design

Create a small shared health-snapshot module in `ff-terminal` (or a lower crate if the gateway will consume it immediately) with these stable types:

```text
FleetHealthSnapshot { observed_at, overall, nodes }
NodeHealth { name, layers[9], overall }
LayerHealth { status: pass|warn|fail|unknown|na, summary, observed_at }
```

Keep collection separate from rendering. The collector should bulk-read Postgres once for durable signals, read Pulse once, then run bounded concurrent network probes. It must not run migrations: doctor is an observation command, and mutating schema makes degraded-database diagnosis less reliable.

Add `fleet: bool` to the existing `Doctor` command. With no flag, retain the current output and exit behavior. With `--fleet`, render one row per node and one column per layer; `--json` serializes the shared snapshot. `--strict` should continue to fail on warnings, while the default exits nonzero only when any applicable layer fails. Unknown data should produce a warning so telemetry loss remains visible.

The header should consume the same snapshot rather than implementing a second health definition. Show a compact `Fleet ●`, `Fleet ▲`, or `Fleet ✕` status with healthy/total node counts; use green/yellow/red and a text glyph so color is not the sole signal. Refresh asynchronously on the existing 30-second cadence, retain the last completed snapshot while a refresh is in flight, and turn it `unknown` after two missed refresh windows. The sidebar can continue showing detailed endpoint dots.

### Severity rules

1. A reachable computer with an absent daemon is a daemon failure, not a reachability failure.
2. An unreachable computer makes network-dependent layers unknown; it must not create eight duplicate failures. The node still fails on reachability.
3. `na` layers do not affect a node's overall status.
4. A stale observation is never a pass.
5. Backup freshness is evaluated only for nodes expected to hold that backup kind and fails when the newest local artifact is missing or older than `2 * interval_secs`.
6. Fleet overall is the worst node status, with `unknown` ranked as warning for display and strict exit semantics.

## Implementation sequence

1. Extract/share reachability and daemon-membership derivation and add the snapshot DTO/status reducer with pure unit tests.
2. Implement bulk collectors for the nine layers, reusing `computer_trust`, `fleet_model_deployments`, `fleet_disk_usage`, `database_replicas`, backup config/alerts, and Pulse state.
3. Add `ff doctor --fleet`, JSON/text rendering, and exit-code tests. Keep the existing `ff doctor` contract unchanged.
4. Replace the TUI's endpoint-only refresh result with (or augment it by) the shared snapshot and add the compact header indicator.
5. Add integration tests guarded to early-return unless `FORGEFLEET_POSTGRES_URL` or `FORGEFLEET_DATABASE_URL` is set, as required for DB-less CI.

## Test plan

- Pure reducer tests for pass/warn/fail/unknown/na and reachability failure suppression.
- Backup boundary tests at exactly and just beyond `2 * interval_secs`, including missing backup and a node not expected to hold that kind.
- Fixture tests for reachable-but-daemon-absent and unreachable-but-recent-heartbeat, ensuring the two columns never collapse.
- Stale/missing sample tests for disk, replica lag, auth probes, and LLM health.
- Text and JSON golden-shape tests for the nine ordered layers; JSON field names should remain stable for automation.
- TUI header tests for healthy, degraded, failed, unknown, and narrow-terminal rendering.
- Optional Postgres collector tests must use the mandated environment-variable early return.

## Schema decision

Live schema inspection confirms usable columns in `computers`, `computer_trust`, `fleet_model_deployments`, `fleet_disk_usage`, `database_replicas`, `backups`, and `fleet_backup_config`. Therefore this work should add **no migration**. LLM restart-count telemetry is a separate observability enhancement; if pursued later, it must use one new forward-only migration at the then-next version rather than changing an existing migration.

## Acceptance criteria

- `ff doctor --fleet` prints exactly nine named per-node layers and supports `--json` and `--strict`.
- A powered-on node with a stopped daemon reads `reachable / daemon absent`.
- Backup freshness uses each kind's configured interval and expected holders, and alerts after `2x` on every node, including fan-out destinations.
- The top bar uses the same aggregate result as the CLI and becomes stale/unknown when refreshes stop.
- Existing `ff doctor` and `ff fleet health` output contracts remain compatible.
- `cargo check --workspace` and the new non-DB tests pass; DB tests skip cleanly when neither supported database URL variable is set.
