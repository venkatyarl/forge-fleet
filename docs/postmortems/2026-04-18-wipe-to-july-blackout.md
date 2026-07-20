# Post-mortem: the Apr-18 wipe, the key-rotation response, and the July blackout

**Status:** draft, compiled from repo evidence (commit history, code comments, migrations).
**Owner:** ops / fleet maintainers.
**Period covered:** 2026-04-17 through 2026-07-20.

## Why these three incidents are one document

Three separate-looking incidents share a single root pattern: the fleet trusted
*stored* state instead of *verified* state — an on-disk backup nobody had tried to
restore, credentials nobody was tracking the age of, and a `computers.status`
column that reflected the last write instead of live pulse freshness. Each
incident was closed by replacing an assumption with an active proof mechanism.
This doc tells the story chapter by chapter and lines up each chapter with the
commits that were its actual response.

---

## Chapter 1 — The Apr-18 wipe

**What happened.** On 2026-04-17 at 18:13 (`57d8b31f`, Taylor Oclaw), the fleet
consolidated Postgres + Redis + Sentinel into a single unified
`deploy/docker-compose.yml` stack with named volumes
(`forgefleet-postgres-data`, `forgefleet-redis-data`,
`forgefleet-sentinel-data`). The following day, 2026-04-18, that consolidation
wiped the fleet metadata DB. This is recorded directly in the codebase — it is
the motivating incident cited in
[`crates/ff-agent/src/ha/restore_drill.rs`](../../crates/ff-agent/src/ha/restore_drill.rs):

> "on 2026-04-18 a docker-compose consolidation wiped the fleet metadata DB.
> Backups existed in principle, but nothing ever proved they could be
> decrypted, extracted, and loaded — a 'backup' that has never been
> test-restored is a liability, not a safety net."

**Impact.** Fleet metadata (registry state, computers, PM data) was lost.
Backups existed on disk, but restorability had never been exercised end to
end, so the wipe briefly left the fleet unable to prove it *could* recover,
only that it had files that plausibly represented a backup.

**Root cause.** A destructive compose operation (a volume/stack
recreation implied by "consolidation") ran against the live metadata store
with no restore-drill safety net and no pre-flight confirmation gate on
fleet-wide destructive operations.

**Immediate response.** Two days later, `ff fleet disband --yes
--i-know-what-im-doing` shipped (`b1ba29a9`, 2026-04-21) — a subcommand that
walks every non-canonical computer row through the same `remove_computer_core`
path used by `remove-computer`, prints the full target list before any delete
runs, and requires both flags before touching data. It exists specifically so
a full-fleet rebuild-from-scratch is a deliberate, confirmed, logged operation
rather than an accidental side effect of infrastructure changes — the
same day, source-tree/bootstrap hardening (`94509295`, V31/V32) also landed to
make node re-enrollment after a wipe reliable.

**Long-term fix.** On 2026-06-15, the scheduled backup **restore-drill**
shipped (`#372`, `crates/ff-agent/src/ha/restore_drill.rs`), explicitly framed
in-code as closing "prod-readiness GAP #6." It runs daily on the leader,
takes the newest `pg_basebackup` archive through checksum → decrypt → extract
→ structural-PGDATA validation, records the outcome to `backup_drills`, and
fires the `backup_restore_drill_failed` alert policy (seeded in migration
V130) if the newest successful drill is more than 2 days stale. The doc
comment on the alert path names the wipe directly: *"a backup that cannot be
restored is a silent data-loss risk (cf. the 2026-04-18 wipe)."*

---

## Chapter 2 — Key rotation

**What happened.** The wipe was treated as more than a data-loss event — it
was treated as a possible integrity/compromise event, because a stack
recreation that can silently destroy the metadata DB is also a stack
recreation that could have exposed or displaced credentials. The response
landed the same week, in the Phase 11 "Security hardening" track of
`d8e477ae` (2026-04-19 18:36):

- **`SecretsRotator`** (`crates/ff-agent/src/secrets_rotation.rs`) — every row
  in `fleet_secrets` gained `expires_at`, `rotate_before_days` (default 90),
  `rotation_count`, and `last_rotated_at` (migration `SCHEMA_V17_SECURITY_HARDENING`,
  `crates/ff-db/src/schema.rs:1305`). Rotation auto-generates new random
  values for `.token` / `.password` / `.key`-suffixed secrets. Exposed via
  `ff secrets rotate` / `ff secrets expirations`.
- **`SshKeyManager`** (`crates/ff-agent/src/ssh_key_manager.rs`) —
  `revoke_computer_trust` fans a revocation out across every alive peer over
  SSH and records it in the new `fleet_ssh_revocations` table (who, which
  fingerprint, from which target node, success/failure). Full keypair
  rotation (`rotate_computer_keypair`) was stubbed at the time and is still
  marked as a multi-step workflow not fully wired
  (`crates/ff-agent/src/ssh_key_manager.rs:205-215`).
- **Pulse HMAC** — every pulse beat is now HMAC-SHA256 signed/verified via a
  `KeyCache` with a 5-minute refresh; readers and the materializer reject
  tampered beats, with backwards compatibility if a key is missing. `ff fleet
  rotate-pulse-hmac` was added to rotate the signing key.
- **Backup encryption** — an age-X25519 keypair is now auto-provisioned in
  `fleet_secrets` on first backup; `pg_basebackup` and the Redis RDB are
  piped through `age -r <pub>` before being written to disk (the same
  encrypted archives the Chapter 1 restore-drill later learned to prove
  restorable).

**Why this matters as a distinct chapter, not just cleanup:** it is the
gap between Chapter 1 and Chapter 3. The wipe forced the fleet to stop
treating credentials as static, unaudited values with no expiry and no
revocation trail — the same "assume it's fine because it's stored somewhere"
failure mode that caused the wipe itself. `SshKeyManager::rotate_computer_keypair`
remaining a stub is a known open gap from this chapter (see Follow-ups).

---

## Chapter 3 — The July blackout

**What happened.** Across roughly 2026-07-19 19:27 through 2026-07-20 02:45,
a dense burst of ~10 commits landed fixing observability and alerting gaps
that, taken together, describe a period where the fleet's own monitoring was
not trustworthy — it could be down without anyone being told:

| Time (2026-07-19/20, -0400) | Commit | Gap closed |
|---|---|---|
| 19:27 | `74912117` | No alert deduplication in `ff-observability` — repeat firings, or none, depending on how the caller behaved. |
| 20:07 | `faf20671` | `computers` row upsert in the pulse materializer wasn't atomic — a race could leave a node's row in an inconsistent state. |
| 20:14 | `72a25579` | No Telegram alerting for *novel* (unclassified) errors in `ff-agent` — new failure modes could occur silently. |
| 22:14 | `6f0af80b` | No scheduled full-mesh SSH reachability check (`ff fleet ssh-mesh-check`) — a broken node-to-node path had no probe. |
| **22:20** | `1927b98c` | **Both Postgres replicas could die silently** — Pulse beats and host-liveness kept flowing from the *hosts*, but nothing checked the replicas themselves, so the failover manager's ODOWN gate never tripped. Fixed by a new leader-gated `replica_monitor` (`crates/ff-agent/src/ha/replica_monitor.rs`) that TCP-probes every registered replica every 60s and fires `postgres_replica_dead` (alert policy seeded by migration V179). |
| 22:42 | `0f48ba96` | No Telegram alert throttling — alert storms (or silence, if a channel choked) were both possible. |
| 23:16 | `595b81bf` | No per-node stale-backup alerting — a node whose backups had quietly stopped landing had no signal. |
| **00:26** | `e4bd4777` | **`computers.status` was a stale stored value, not derived from live pulse freshness** — a dashboard could keep showing a node as online long after it had actually gone dark. |
| 02:33 | `eb60b278` | Gateway alert handling updated. |
| 02:45 | `bd9e8ed7` | Slot-level locking added to `ff-pulse`. |

The two commits in bold are the core of the blackout: the fleet's *health
signal itself* had two independent blind spots at once — dead replicas that
produced no alert, and a status column that could report "alive" from stale
data rather than a live check. Combined, this means there was a window where
the fleet could be materially unhealthy (both PG replicas down) while every
dashboard and alert channel stayed quiet.

**Root cause.** Monitoring had been built incrementally, tier by tier
(host liveness via Pulse beats, then alert policies, then Telegram delivery),
without a pass that verified the *composition* held: that "host up" did not
imply "replica up," and that "row says online" did not imply "pulse says
fresh." Both gaps are the same category of bug as Chapters 1 and 2 — stored
state (`computers.status`, "replica presumably following the primary")
substituting for a live check.

**Resolution.** All ten gaps above were closed in the same overnight window
via the fleet's own automated work-item dispatch (commit trailers read
"Automated work_item dispatch (ForgeFleet Pillar 4)"), i.e. the fleet's
distributed build fixed its own blind spot once the gaps were identified and
queued as work items. `replica_monitor` and the `computers.status`
pulse-freshness fix are the two that directly restore signal; the rest
(dedup, throttling, novel-error alerting, mesh-check, atomic upserts,
gateway handling, pulse slot locking) harden the pipeline around them so the
same class of silent gap is less likely to recur.

---

## Cross-cutting lessons

1. **A backup, a credential, and a status column all fail the same way**:
   by being trusted without being actively re-verified. Every durable fix
   across all three chapters follows the same shape — turn a passive stored
   value into a scheduled, alerting proof (restore-drill, rotation +
   expiry tracking, replica TCP probes, freshness-derived status).
2. **Destructive fleet-wide operations now require explicit double
   confirmation** (`ff fleet disband --yes --i-know-what-im-doing`), a
   direct consequence of Chapter 1.
3. **"Host is up" is not "service is up."** Chapter 3's core bug was
   conflating Pulse host-liveness with Postgres replica health. Any future
   HA component should be monitored at the service level it actually
   guarantees, not inferred from a broader liveness signal.
4. **Automated remediation velocity is a strength but not a substitute for
   a composed monitoring review.** All ten July gaps were fixed within
   hours once queued, but they had likely coexisted, unnoticed, for longer —
   the fix here is a good response, not evidence the gap was caught quickly.

## Open follow-ups

- `SshKeyManager::rotate_computer_keypair` is still a stub
  (`crates/ff-agent/src/ssh_key_manager.rs:205-215`) — Chapter 2's rotation
  story only covers `fleet_secrets` values and SSH *trust revocation*, not
  full keypair rotation.
- No equivalent "restore-drill"-style scheduled proof exists yet for pulse
  HMAC key rotation or SSH revocation correctness — both are fire-and-forget
  today.
- This document is compiled from commit history and in-code motivation
  comments; it does not include externally-tracked facts such as exact
  user-visible downtime duration or who was paged during the July blackout.
  If those are recorded outside this repo (incident channel, on-call log),
  fold them in here.
