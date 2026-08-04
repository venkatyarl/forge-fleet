# Coder-model routing and remaining Devstral assignments

Audit snapshot: **2026-08-04 00:03-00:06 UTC**, repository
`af4fb9a24a54fdc37a4f0b4ec56ebf81ed5dcac9`. This audit was read-only. It did
not load, unload, replace, or reconfigure a model.

## Result

The canonical code router has no ordering defect and needs no code change. It
returns every cataloged, active, healthy, fresh, 32K code-capable deployment:
eight GLM-4.5 Air endpoints. `code`, `codegen`, and `review` produced the same
eight-node set; least-loaded mode changed only their order.

The remaining work is catalog and deployment reconciliation:

- The completed rollout is visible as GLM on nine directly reachable nodes.
  Eight have authoritative deployment rows. Adele serves GLM on `:55008`, but
  that process has no `fleet_model_deployments` row and is therefore invisible
  to routing. Adele also still serves a cataloged Devstral on `:55000`.
- Sia was recorded by the shared workstream as locally serving GLM, but none of
  `:55000`, `:55001`, `:55002`, or `:55008` was reachable through its registered
  LAN IP during this audit, and it has no current GLM deployment row. Do not
  represent this as externally routable until both facts are repaired.
- Marcus, Sophie, and James are the intentional Devstral assignments. Their
  31/31/61 GB hosts cannot fit the 73.5 GB GLM artifact.
- `qwen3-coder-next` is absent from the live catalog. Veronica has one cold
  97,829,376,032-byte library object recorded as
  `unknown:qwen3-coder-next-80b`; it is not deployable or routable by canonical
  identity.
- GLM-5.2 is a tier-4 active catalog row with a 254 GB variant and one cold
  253,878,401,856-byte copy on Beyonce. Every online candidate host has at most
  123 GB RAM, so it is not a valid single-node assignment.

## Exact live catalog evidence

The authoritative query was against `fleet_model_catalog` by exact/substring
identity. Relevant rows were:

| Catalog ID | Tier | Size | Declared workloads | Tool calls | Reconciliation |
|---|---:|---:|---|---|---|
| `glm-4.5-air` | 2 | 73.5 GB Q4_K_M | agentic, code, reasoning, tool_calling | true | Correctly code-routable |
| `devstral-small-2-24b` | 2 | 14 GB UD-Q4_K_XL | reasoning, tool_calling | true | Not declared code-capable; excluded from constrained code routes |
| `glm-5.2` | 4 | 254 GB UD-Q2_K_XL | agentic, code, coding, reasoning, tool_calling | true | Active metadata, but no single fleet node can fit it |
| `qwen3-coder-next` | - | - | - | - | Missing from the live catalog |

The live catalog and checked-in history differ in two important ways.
`SCHEMA_V113_CODER_TOOL_CALLING` mentions `qwen3-coder-next` only in an
`UPDATE`; it does not create a missing row. `SCHEMA_V223_REAL_SIZED_MODEL_CATALOG`
originally seeded GLM-5.2 as a gated, unsized watch candidate with no artifact.
The live row is now sized and active, but its 254 GB variant does not declare
the multi-node/offload-only placement constraint used by other oversized
catalog entries. These are catalog-policy gaps, not router-order gaps.

## Deployment rows reconciled with direct probes

The authoritative deployment query joined `fleet_model_deployments` to
`fleet_workers`, filtered the four model identities, and inspected
`desired_state`, health timestamps, context, and verified profile timestamps.
At 00:03-00:04 UTC it contained eight healthy active GLM rows and four healthy
active Devstral rows. All reported `context_window=32768` and
`usable_agent_ctx=32768`.

`/v1/models` was then fetched directly from every expected listener, so an open
port or database label was never treated as model identity.

| Node | RAM | DB deployment | Direct served identity | Reconciliation |
|---|---:|---|---|---|
| Adele | 122 GB | Devstral `:55000` | Devstral `:55000`; GLM `:55008` | GLM row missing; Devstral remains live after rollout |
| Thalia | 122 GB | GLM `:55000` | GLM `:55000` | Converged |
| Sia | 122 GB | none for these four models | registered ports unreachable | Local workstream claim is not externally routable evidence |
| Rihanna | 122 GB | GLM `:55008` | GLM `:55008` | Converged |
| Beyonce | 122 GB | GLM `:55000` | GLM `:55000` | Converged; cold GLM-5.2 must not be started here |
| Shakira | 122 GB | GLM `:55000` | GLM `:55000` | Converged |
| Logan | 123 GB | GLM `:55000` | GLM `:55000` | Converged |
| Veronica | 123 GB | GLM `:55008` | GLM `:55008` | Converged; coder-next remains cold and non-canonical |
| Lily | 123 GB | GLM `:55008` | GLM `:55008` | Converged |
| Duncan | 123 GB | GLM `:55008` | GLM `:55008` | Converged |
| Marcus | 31 GB | Devstral `:55008` | Devstral `:55008` | Intentional small-host assignment |
| Sophie | 31 GB | Devstral `:55008` | Devstral `:55008` | Intentional small-host assignment |
| James | 61 GB | Devstral `:55008` | Devstral `:55008` | Intentional small-host assignment |

The GLM identity was
`zai-org_GLM-4.5-Air-Q4_K_M-00001-of-00002.gguf`; the Devstral identity was
`Devstral-Small-2-24B-Instruct-2512-UD-Q4_K_XL.gguf`. Real chat-completion
probes to Logan GLM and Marcus Devstral both returned exactly
`ROUTE_AUDIT_OK`. This proves both model families were generating, not merely
answering health endpoints.

Legacy/status projections are not authoritative for assignment counts. `ff status`
displayed duplicate and stale-looking model/port combinations on
several nodes. The deployment table plus direct identity probe is the
reconciled source for this audit.

## Router behavior

The canonical constrained route is:

```text
fleet_oneshot_for[_ctx]
  -> resolve_route_candidates
  -> ff_db::pg_route_deployments
  -> rank_candidates
```

`pg_route_deployments`:

- normalizes code requests across `code`, `code-gen`, `codegen`, `coder`,
  `coding`, `code-generation`, `review`, `code-review`, and `reviewer`;
- joins deployment and catalog rows and requires `health_status='healthy'`,
  `desired_state='active'`, and a non-retired catalog entry;
- enforces the requested workload, `tool_calling`, per-slot usable context,
  exclusions, and maximum health age;
- orders by ascending tier, then optional request/CPU load, then health
  freshness.

`resolve_route_candidates` uses a 180-second freshness ceiling and fails closed
for a requested workload/context contract. `rank_candidates` preserves the
database order while preferring available hinted-family slots, then full slots
in that family, then other families. Offload code kinds use the same database
scorer and never fall back from an empty code set to an arbitrary tool model.

Observed route probes were:

```text
ff fleet route code --tool-calling --min-ctx 32768 --limit 30 --format json
ff fleet route code --tool-calling --min-ctx 32768 --limit 30 --least-loaded --format json
ff fleet route codegen --tool-calling --min-ctx 32768 --limit 30 --format json
ff fleet route review --tool-calling --min-ctx 32768 --limit 30 --format json
```

The freshness-ordered decision was Logan GLM. All three workload spellings
returned Logan, Veronica, Duncan, Beyonce, Rihanna, Thalia, Shakira, and Lily
GLM (ordering reflected health time). Least-loaded selected Rihanna first and
returned the same set. Adele is absent solely because its serving GLM lacks a
deployment row; Sia is absent because it lacks a row/reachable endpoint;
Devstral is absent because its live catalog row does not declare `code`.

No router edit is justified. Hard-coding one family ahead of another would
mask missing/incorrect catalog and deployment authority.

## Recommended migrations by node

These are recommendations for a separately authorized deployment/catalog
task; none was executed here.

| Node(s) | Recommended migration | Precondition and rollback check |
|---|---|---|
| Adele | Register the already-serving GLM `:55008`, then retire the redundant Devstral `:55000` assignment if the ten-node replacement policy still applies | Require exact `/v1/models`, successful generation/tool-call probe, 32K usable context, fresh profile row, safe memory pressure, and route visibility. If any fails, keep Devstral and remove/disable only the new GLM row; never leave a row pointing at the wrong process |
| Sia | Repair registered-IP reachability and reconcile the localhost-confirmed GLM process into an active deployment row | Require remote `/v1/models` plus generation, correct port, 32K context, fresh health, and process identity. Roll back advertisement/row on reachability, identity, context, or memory failure; do not unload a working local process merely to repair metadata |
| Logan, Veronica, Lily, Duncan, Rihanna, Beyonce, Shakira, Thalia | No model migration; retain GLM and monitor row/process identity | Roll back any future change if the eight-member route set shrinks, the served artifact changes, health exceeds 180 seconds, context falls below 32K, or memory pressure rises |
| Marcus, Sophie | Retain Devstral; GLM cannot fit in 31 GB | Before any smaller-coder replacement, require canonical catalog `code` and tool metadata, 32K probe, generation/tool call, and preserve Devstral until the replacement is routable |
| James | Retain Devstral unless an approved smaller coder replaces it; GLM cannot fit in 61 GB | Same checks as Marcus/Sophie; roll back to Devstral on identity, context, tool-call, health, or memory regression |
| Veronica coder-next copy | Keep cold. First create one canonical catalog identity, reconcile the `unknown:` library row, validate license/artifact/size/tool calls, and measure headroom before any canary | A 97.8 GB file leaves only about 25 GB before runtime/context overhead on a 123 GB host. Abort/roll back on unsafe available RAM, swapping, wrong identity, or loss of the existing GLM fallback |
| Beyonce GLM-5.2 copy | Keep cold; do not attempt single-node activation. Add an explicit `offload_only`/`multi_node_ring_only` placement constraint before planning a ring canary | Require aggregate ring capacity plus per-node headroom, exact shard/artifact identity, bounded context, generation/tool probe, and an untouched GLM fallback pool. Tear down only the canary on any failed gate |

## Stop/rollback gates

For every future wave, snapshot the existing deployment rows and direct served
identities first. Stop the wave and restore the prior row/process assignment on
any of: wrong model identity; missing or stale health; externally required but
failed reachability; usable context below 32768; tool-call or real-generation
failure; unsafe RAM/swap pressure; duplicate active identity on one port; or a
reduction in the established eight healthy GLM code-route candidates. Database
reconciliation must follow process proof, never precede it.

An independent `codex,kimi` council was also attempted. Codex agreed that this
is catalog/deployment reconciliation rather than a router-code defect; Kimi
failed to return a usable answer, so no two-model consensus is claimed.
