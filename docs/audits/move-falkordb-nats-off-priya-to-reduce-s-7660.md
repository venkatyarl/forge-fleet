# Audit: Move FalkorDB + NATS off priya to reduce SPOF; form 3-node JetStream cluster

- **Work item:** feature/move-falkordb-nats-off-priya-to-reduce-s-7660
- **Audit date:** 2026-07-20 (all live-state claims verified this date from node adele)
- **Verdict:** Both moves are LOW RISK and cheaper than the council assumed — FalkorDB on
  priya is completely empty, and priya's JetStream holds **zero streams** (the stream-init
  code is dead code, see F4). There is no data to migrate for either service. The real work
  is config/provisioning, plus one small code fix to make JetStream replication actually
  matter.

## 1. Live state (verified, not from source comments)

### Fleet topology (`ff db query fleet_workers`, 2026-07-20)

| node | ip | status | notes |
|---|---|---|---|
| priya | 192.168.5.104 | online | runs the whole `deploy/docker-compose.yml` stack |
| marcus | 192.168.5.102 | online | 31 GB linux-ubuntu, target for FalkorDB + NATS peer |
| sophie | 192.168.5.103 | online | 31 GB linux-ubuntu, target for NATS peer |
| taylor | 192.168.5.100 | **offline** since 2026-07-16 | old macOS leader; many stale hardcoded fallbacks still point here (§5) |

### TCP probes (from adele, 2026-07-20)

- priya 192.168.5.104: **55432 (postgres) OPEN, 56379 (redis) OPEN, 63379 (falkordb) OPEN,
  54222 (nats client) OPEN** — the SPOF the council flagged is real and confirmed.
- priya 56222 (nats cluster port): **closed** — single-node NATS, cluster block never enabled.
- marcus 192.168.5.102: 63379 closed, 54222 closed (ports free for the move).
- sophie 192.168.5.103: 63379 closed, 54222 closed.

### FalkorDB content (redis protocol probe against 192.168.5.104:63379)

- `GRAPH.LIST` → empty array; `DBSIZE` → 0.
- **The instance holds zero graphs and zero keys.** The council's "empty + rebuildable,
  zero data risk" assumption is confirmed. The move is a container relocation, not a data
  migration.

### NATS / JetStream state (`http://192.168.5.104:58222/varz` + `/jsz`)

- Server 2.10.29, `server_name: forgefleet-nats-taylor` — **name drift**: the compose stack
  was moved taylor→priya without updating `deploy/docker/nats.conf`.
- JetStream enabled (20 GB file store) but **0 streams, 0 consumers, 0 messages**.
- Only 2 client connections, both from `172.18.0.1` (priya's own docker bridge gateway,
  i.e. priya-local processes). No other fleet node is connected to NATS at all.

## 2. Findings

**F1 — NATS is effectively priya-local today, by silent default.**
Every consumer resolves the URL from `FORGEFLEET_NATS_URL` with fallback
`nats://127.0.0.1:54222` (`crates/ff-agent/src/nats_client.rs:18`,
`crates/ff-pulse/src/nats.rs:20`, `crates/ff-terminal/src/{logs_cmd.rs:10,events_cmd.rs:8}`).
Nothing provisions that env var: `scripts/bootstrap-computer-template.sh`, `scripts/forgefleetd`,
and the deployed systemd unit (`forgefleetd.service` on adele, inspected live) set FF_NODE /
FORGEFLEET_NODE_NAME but **not** FORGEFLEET_NATS_URL, and `~/.forgefleet/fleet.toml` has
`[database]` and `[redis]` sections but no `[nats]`. So on every node except priya the daemon
tries localhost, fails, and silently no-ops (NATS is optional by design,
`src/main.rs:141-156`). Clustering NATS without fixing URL provisioning changes nothing for
fleet resilience — clients must learn all three seed URLs.

**F2 — `fleet_backup_config` pins falkordb to priya (live row confirmed).**
`SELECT` shows `('falkordb', source_host='priya', 21600s, enabled)`. The backup runner
(`crates/ff-agent/src/ha/backup.rs`) only runs the falkordb dump on the node matching
`source_host` and shells into the container by fixed name `forgefleet-falkordb`
(`backup.rs:68`). After the move this row must say `marcus` or backups silently stop
(priya no longer has the container; marcus never matches).

**F3 — No live code consumes a FalkorDB endpoint.**
`FalkorCortexGraphStore` (`crates/ff-brain/src/cortex/storage.rs:280`) is a scaffold: it is
constructed nowhere in the workspace and every query method bails "result decoding is not
wired yet". Live Cortex reads/writes Postgres. So "update cortex/brain FalkorDB endpoint"
amounts to: compose file + backup-config row + plan doc (`plans/cortex-falkordb-backend.md`).
There is no `[falkordb]`/`FORGEFLEET_FALKORDB_URL` config key anywhere — one should be added
when the backend is actually wired (recommendation R4).

**F4 — `init_jetstream_streams` is dead code; FF_TASKS does not exist.**
`crates/ff-agent/src/nats_jetstream.rs` defines the AUDIT/COST/ALERTS/FF_TASKS/LOGS streams
but `init_jetstream_streams` has **zero callers** (grep across workspace) — matching the live
`/jsz` showing 0 streams. `publish_task_inserted` (task_runner.rs:904,976) works only because
core-NATS publish doesn't require the stream; nothing is durable. Additionally the stream
configs don't set `num_replicas`, so even once called they'd create R1 streams — and
`get_or_create_stream` does **not** upgrade replicas on an existing stream. The work item's
goal "FF_TASKS survives a node loss" requires wiring this call into daemon startup with
`num_replicas: 3`.

**F5 — Cluster scaffolding exists but is stale.**
`deploy/docker/nats.conf` has the cluster block commented out with placeholder routes
`marcus.local:6222` / `sophie.local:6222` (mDNS names; the fleet uses static IPs) and a
hardcoded `server_name: forgefleet-nats-taylor`. Compose maps 56222→6222 already. Note the
conf's own warning: an orphan cluster block with no routes makes JetStream refuse to start —
the cluster must be enabled with real routes on all three nodes in one operation.

## 3. FalkorDB → marcus runbook (do first; ~15 min, zero data risk)

Target endpoint: **192.168.5.102:63379** (browser UI 63000). Ports verified free on marcus.

1. On marcus: clone/pull forge-fleet, then `cd deploy && docker compose up -d falkordb`
   (compose supports per-service start; the rest of the stack stays on priya).
   Verify: `redis-cli -h 192.168.5.102 -p 63379 ping` → PONG.
2. Repoint backups (live DB, config is DB-driven by design — no code change):
   `UPDATE fleet_backup_config SET source_host='marcus', updated_at=NOW() WHERE kind='falkordb';`
   Optionally codify as a forward-only migration (next free version is **V180**; live
   `_migrations` max = 179 = source max V179 — re-check both branches for collisions first).
3. On priya: `docker compose stop falkordb && docker compose rm -f falkordb` and remove the
   empty volume `docker volume rm forgefleet-falkordb-data`. Safe: GRAPH.LIST/DBSIZE are 0
   (re-verify immediately before deleting).
4. Doc/comment updates in-repo: `deploy/docker-compose.yml` header (lines 12-15 still say
   192.168.5.100 — taylor, offline; falkordb line should say 192.168.5.102),
   `plans/cortex-falkordb-backend.md` endpoint references,
   `crates/ff-db/src/schema.rs` V163 comment is historical — leave it (forward-only rule).
5. Verify: port probe 192.168.5.102:63379 OPEN, 192.168.5.104:63379 closed; next backup tick
   produces `~/.forgefleet/backups/FalkorDB/…` on marcus, not priya.

Rollback: reverse steps (start container on priya, UPDATE row back). No data either way.

## 4. 3-node JetStream cluster runbook (priya + marcus + sophie)

Prereq check done: all three are linux-ubuntu with docker-capable roles; 54222/56222 free on
marcus + sophie. JetStream store on priya is empty, so there is **no stream migration** —
greenfield cluster formation.

1. **Per-node nats.conf** (replace the single shared `deploy/docker/nats.conf`; template it
   with `server_name: forgefleet-nats-<node>` — unique names are mandatory in a cluster, and
   this also fixes the `-taylor` drift):

   ```
   server_name: forgefleet-nats-priya        # marcus / sophie per node
   listen: 0.0.0.0:4222
   http_port: 8222
   jetstream { store_dir: /data/jetstream, max_memory_store: 1GB, max_file_store: 20GB }
   cluster {
       name: forgefleet-cluster
       listen: 0.0.0.0:6222
       # Docker bridge + port map (56222→6222) hides the real address, so each node
       # MUST advertise its LAN ip:hostport or routes will gossip 172.18.x addrs:
       advertise: <node-lan-ip>:56222        # e.g. 192.168.5.104:56222 on priya
       routes: [
           nats-route://192.168.5.104:56222
           nats-route://192.168.5.102:56222
           nats-route://192.168.5.103:56222
       ]
       # (a route to self is ignored; identical routes list on all 3 nodes)
   }
   ```
   Simpler alternative on these Linux hosts: `network_mode: host` for the nats service, which
   removes the advertise/port-map subtlety entirely (then use native 4222/6222/8222 — but that
   breaks the 5-digit canonical-ports convention, so the advertise approach above is preferred).
2. Bring up nats on marcus + sophie (`docker compose up -d nats` with their conf), then
   restart priya's nats with the cluster block. Verify `/varz` `cluster.urls` shows 2 routes
   on each node and `/jsz` reports `meta_cluster` with 3 peers and an elected leader.
3. **Code fix (required for the stated goal):** call
   `ff_agent::nats_jetstream::init_jetstream_streams` at daemon startup (after the
   `init_nats` in `src/main.rs:146`), and set `num_replicas: 3` in the stream `Config`
   (gate on cluster size, e.g. env `FORGEFLEET_NATS_REPLICAS`, so a dev single-node
   still works — R>1 on a single node errors). Streams don't exist yet, so no
   update-existing-stream handling is needed if this lands with/after the cluster.
4. **Provision the client URL fleet-wide (fixes F1):** set
   `FORGEFLEET_NATS_URL=nats://192.168.5.104:54222,nats://192.168.5.102:54222,nats://192.168.5.103:54222`
   (async-nats `ToServerAddrs` accepts comma-separated URLs; client fails over automatically).
   Wire it where the fleet already wires env: `scripts/bootstrap-computer-template.sh`, the
   `forgefleetd` systemd unit template, and add a `[nats] urls` key to `~/.forgefleet/fleet.toml`
   with a `resolve_nats_url()` fallback chain (env → fleet.toml → localhost default), mirroring
   `fleet_events::resolve_redis_url`.
5. Verify node-loss survival: `docker compose stop nats` on priya → publishes via marcus/sophie
   still ack; FF_TASKS `/jsz` shows leader re-election; restart priya, peer catches up.

## 5. Sequencing + out-of-scope notes

- **Per council 2026-07-18: Postgres streaming replicas remain priority #1.** Nothing above
  touches postgres/pgcat on priya; the FalkorDB move (§3) is a 15-minute independent task and
  must not displace the replica work. The NATS cluster (§4) is also independent but includes a
  code change (step 3) that should ride a normal PR.
- **Stale taylor (192.168.5.100) endpoints found while auditing** — each is a latent outage
  since taylor is offline; file as follow-up items:
  - `crates/ff-agent/src/fleet_events.rs:35` — redis fallback `redis://192.168.5.100:56379`
  - `crates/ff-agent/src/main.rs:202` — gateway fallback `http://192.168.5.100:51002`
  - `scripts/lib/fleet.sh:63` — PGURL fallback `…@192.168.5.100:55432/…`
  - `deploy/docker-compose.yml:12-16` header comments; `scripts/deploy-to-fleet.sh:113`
- Redis (56379) also stays on priya for now; its failover story is Pulse P2P (Plan 14), not
  this item.

## 6. Answers to the work item's explicit asks

| Ask | Answer |
|---|---|
| Move forgefleet-falkordb to marcus | Runbook §3. Confirmed empty (0 graphs / 0 keys) — container relocation only. |
| Update cortex/brain FalkorDB endpoint → marcus:63379 | No live code consumes an endpoint (F3). Update: compose header/service docs, `fleet_backup_config.source_host` row, `plans/cortex-falkordb-backend.md`. Add a real `[falkordb]` config key when the Falkor backend is wired (F3/R4). |
| 3-node JetStream cluster, 3 stream replicas | Runbook §4. Blocker to the stated goal: `init_jetstream_streams` is never called and omits `num_replicas` (F4) — small code change required, otherwise the cluster protects nothing. |
| FF_TASKS survives node loss | Only after §4 step 3; today FF_TASKS does not exist as a stream. |
