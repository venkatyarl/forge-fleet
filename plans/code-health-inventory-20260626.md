# Code-health inventory (2026-06-26)

Findings from a deep discovery pass over the work-dispatch, gateway, and
self-heal subsystems during the autonomous build loop (iteration 52, after the
cloud-routing + reaper + substring-classifier bug series #589–#595). The
actively-used paths are in good shape; this records what is LIVE vs DORMANT vs
DEAD so future work builds on the right system (the CLAUDE.md discovery-first
mandate) rather than reviving a vestigial one.

## Three parallel work systems — know which is live

| System | Table(s) | Status | Owner code |
|--------|----------|--------|-----------|
| **PM / Pillar-4** | `work_items` (+ `work_item_leases`, `work_item_merge_queue`) | **LIVE** | ff-mc + `work_item_scheduler` / `work_item_dispatch` / `lease_takeover` / `work_item_merge_drain`; `ff pm` |
| **fleet_tasks** | `fleet_tasks` | **LIVE** | `task_runner` (heartbeat watchdog), `batch_manager` (decomposed parents); `ff tasks` |
| **fleet_work_items** | `fleet_work_items` | **DORMANT** (0 rows live) | `work_stealer` (`spawn_steal_loop` / `spawn_work_item_watchdog`, wired in src/main.rs) + `batch_manager::complete_finished_parents` |

`work_stealer` and `batch_manager::complete_finished_parents` operate ONLY on
`fleet_work_items`, which is empty in production — the real decomposed-work path
runs through `fleet_tasks`. The work-stealing fair-share/handoff logic is wired
into the daemon but exercises an empty table. Before extending decomposed-task
handling, confirm which table the feature targets.

## Dead-exported modules (0 callers anywhere)

- `ff_agent::consensus` (`run_consensus` / `ConsensusConfig` / `ConsensusResult`)
  — `pub mod consensus;` in lib.rs, but no caller in the workspace. It is a
  DISTINCT approach (N agents solve the same coding task, pick best by tests),
  NOT the same as the live `ff council` (multi-model opinion synthesis), so it's
  a never-wired feature rather than a strict duplicate. It also has a latent
  bug: every agent gets the same `working_dir` (no per-agent worktree
  isolation), so the N solutions would clobber each other. Decision deferred to
  operator: finish (add isolation + a caller) or delete.
- `ff_core::quarantine` (`NodeQuarantine` / `QuarantinePolicy` / `QuarantineEntry`)
  — exported via `pub use`, no live caller. The live failure-tracking /
  circuit-breaking is `ff-agent::circuit_breaker` + `ff-api` router, not this.
  Sliding-window `FailureTracker` logic looks correct but is untested + unused.

## Verified hardened (clean, well-tested — no change needed)

- `heartbeat_v2::pick_primary_lan_ip` — wired-beats-wifi canonical-IP selection
  (source of the aura/ace IP outages); 4 tests cover ethernet/thunderbolt/wifi/empty.
- `peer_map` — `count_alive` / `stale_names` are exact complements; 5 tests incl.
  injectable-now.
- `model_id::normalize_model_id` — loop-based quant strip + dash folding; 9 tests.
- `batch_manager` — `percent_complete` guards `total==0`; weighted-partition tested.

## Recurring bug classes already cured (for reference)

- Param-size substring trap → `orchestrator_adapter::parse_params_b` (#595) is the
  reference cure (read the real `<number>b` token; don't substring a fixed list).
- Reaper stale-window < worker max-runtime → couple the consts with a test (#590/#591).
- Cloud model routing namespace → `bare_model_name` strips up-to-last-slash (#593/#594).
- OpenAI message `content` may be string OR array → `flatten_text_content` (#592).
