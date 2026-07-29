# Coder-model routing after the 10/10 GLM rollout

Audit snapshot: **2026-07-29 12:45–12:50 UTC**. This is a read-only
reconciliation of:

- the authoritative ForgeFleet workstream result;
- live `fleet_model_catalog` and `fleet_model_deployments` rows;
- the checked-in router at `a61bcfdc`;
- `ff fleet route` output; and
- direct `/v1/models` and generation probes.

No model was loaded, unloaded, replaced, or otherwise changed during this
audit.

## Executive result

The rollout result is authoritative: `glm-4.5-air` was confirmed on all ten
target nodes — Adele, Thalia, Sia, Rihanna, Beyonce, Shakira, Logan, Veronica,
Lily, and Duncan — and replaced Devstral on those nodes. Shakira serves GLM on
port `55000`; the other recorded GLM endpoints use `55008`.

The current snapshot is **not converged with that completed rollout**. Direct
process probes now find GLM on only Thalia, Beyonce, Veronica, Lily, and Duncan.
Adele, Sia, and Rihanna are again serving Devstral on `55000`; Logan has no
listener on `55000` or `55008`; and Shakira was unreachable over its registered
LAN address. Live Postgres agrees with the first three regressions and Logan's
absence, but retains an obsolete stale Shakira row on `55008`.

Therefore, “10/10” is preserved as a confirmed rollout result, not misreported
as the present serving count. The current mismatch is post-rollout drift or
rollback and catalog reconciliation lag. Fixing it requires a separately
authorized deployment/reconciliation operation.

The route code is sound for its declared policy, but it can only select healthy,
fresh rows in Postgres. At this snapshot the constrained code route exposes
four GLM nodes (Beyonce, Veronica, Lily, Duncan) plus Beyonce's tier-3
Qwen3-Coder endpoint. It cannot route to successfully rolled-out nodes whose
deployment rows were restored to Devstral, removed, or left stale.

## Reconciliation with the 2026-07-28 audit

The prior audit at `5e51044fe93018b5a0f9367c05e95d18ddcb4ff5` found six
live Devstral assignments, five reachable GLM assignments, and one
stale/unreachable Shakira GLM row. It correctly described the state at its
2026-07-28 20:12–20:14 UTC snapshot, but its conclusion that the workstream's
10/10 claim was inconsistent is now superseded by the later, cross-verified
rollout result.

The workstream records the causes of the earlier undercount:

1. probes assumed `55008`, while Shakira serves GLM on `55000`;
2. Adele had intermittent network failures; and
3. a deployment row-loss bug, fixed in `5d6b5fbf`, made catalog-only counts
   incomplete.

That evidence establishes that all ten replacements did occur. It does not
override the direct process identities observed in this newer snapshot.

## Evidence matrix

| Node | Authoritative rollout | Live Postgres at audit | Direct process probe at audit | Reconciliation |
|---|---|---|---|---|
| Adele | GLM, Devstral replaced | Devstral `:55000`, healthy | Devstral `:55000` | Post-rollout regression |
| Thalia | GLM | GLM `:55008`, healthy, `12288×4` | GLM `:55008` | Serving GLM; below 32K agent context |
| Sia | GLM, Devstral replaced | Devstral `:55000`, healthy | Devstral `:55000` | Post-rollout regression |
| Rihanna | GLM, Devstral replaced | Devstral `:55000`, healthy | Devstral `:55000` | Post-rollout regression |
| Beyonce | GLM | GLM `:55008`, healthy, `32768×1` | GLM `:55008` | Converged |
| Shakira | GLM `:55000` | GLM `:55008`, stale since July 27 | registered LAN IP unreachable | Port row is obsolete; current process not independently reachable |
| Logan | GLM | no GLM deployment row | neither `:55000` nor `:55008` listens | Post-rollout regression |
| Veronica | GLM | GLM `:55008`, healthy, `32768×1` | GLM `:55008` | Converged |
| Lily | GLM | GLM `:55008`, healthy, `32768×1` | GLM `:55008` | Converged |
| Duncan | GLM | GLM `:55008`, healthy, `32768×1` | GLM `:55008` | Converged |

The direct `/v1/models` responses identified artifacts, not just open ports.
GLM returned
`zai-org_GLM-4.5-Air-Q4_K_M-00001-of-00002.gguf`; the regressed nodes returned
`Devstral-Small-2-24B-Instruct-2512-UD-Q4_K_XL.gguf`. A direct Beyonce
generation returned `GLM_OK`, with the expected separate
`reasoning_content`, at a 128-token budget.

The catalog still has complete tier-2, tool-calling code entries for
`glm-4.5-air` and `devstral-small-2-24b`. As in the prior audit,
`qwen3-coder-next` is absent and `glm-5.2` remains a non-deployable watch row;
neither affects the completed GLM rollout.

## Direct routing probes

The constrained probe was:

```text
ff fleet route code --tool-calling --min-ctx 32768 --limit 20 --format json
```

It returned, in stable tier/freshness order:

1. Beyonce GLM;
2. Veronica GLM;
3. Lily GLM;
4. Duncan GLM; and
5. Beyonce Qwen3-Coder-30B.

The same probe with `--least-loaded` chose Duncan GLM, then Veronica, Lily, and
Beyonce GLM, followed by Beyonce Qwen3-Coder. Adele, Sia, Rihanna, and the
hardware-policy Devstral nodes were excluded from this constrained route
because their cataloged deployment state did not meet the 32K code-agent
selection at audit time. Thalia was excluded because its per-slot usable
context is only 12288. Stale Shakira and absent Logan were correctly excluded.

## Route-code verification

The canonical path remains:

```text
fleet_oneshot_for[_ctx]
  -> resolve_route_candidates
  -> ff_db::pg_route_deployments
  -> rank_candidates
```

Current source verifies all of the following:

- Code synonyms include `code`, `code-gen`, `codegen`, `coder`, `coding`,
  `code-generation`, `review`, `code-review`, and `reviewer`.
- `pg_route_deployments` joins the deployment and catalog tables; requires
  `health_status='healthy'`; applies workload, tool-calling, per-slot context,
  host-exclusion, and health-age filters; orders by ascending catalog tier; and
  optionally uses live request/CPU load before health freshness.
- `fleet_oneshot_for_ctx` applies a 180-second health ceiling, least-loaded
  ordering, a top-eight pool without a hint, and a widened pool with a hint.
- `rank_candidates` prefers an available hinted family, then full endpoints in
  that family, then other healthy families.
- Moderate and complex local work reads `local_capable_model_hint` and defaults
  to `glm-4.5-air`; mechanical work has no family hint and may select any
  equal-tier healthy coder.
- Context and workload filters deliberately fail open only when their
  constrained candidate set is empty. Health and freshness constraints remain
  active.

No route-code change is warranted by this audit. Hard-coding GLM ahead of
Devstral would hide the catalog drift and contradict the existing
complexity/family-hint and least-loaded policies.

## Remaining Devstral policy

After restoring the confirmed rollout state, Devstral should remain only where
hardware policy requires the smaller 14 GB model:

| Node | RAM | Policy |
|---|---:|---|
| Marcus | 31 GB | Retain Devstral; 73.5 GB GLM cannot fit |
| Sophie | 31 GB | Retain Devstral; 73.5 GB GLM cannot fit |
| James | 61 GB | Retain Devstral unless a smaller approved coder replaces it; GLM exceeds total RAM |

All three currently use `16384×2`, so they are reprofile candidates rather than
32K agent-capable endpoints. Their presence is intentional hardware policy.

Adele, Sia, and Rihanna are **not** policy exceptions: each has 122 GB RAM and
was confirmed running GLM during the rollout. Their current Devstral processes
must be treated as regressions, not accepted steady state.

## Required follow-up

In a separately authorized deployment task:

1. Determine what restored Devstral on Adele, Sia, and Rihanna and removed
   Logan's GLM listener after the confirmed rollout.
2. Restore Shakira reachability, then verify GLM on `55000` and replace the
   obsolete Postgres `55008` row.
3. Reconcile Postgres only after checking `/v1/models` and a real generation on
   each process; do not infer model identity from an open port.
4. Re-run the constrained and least-loaded route probes. The intended result is
   nine 32K GLM route candidates, with Thalia separately reprofiled if 32K agent
   routing is required.
5. Keep Marcus, Sophie, and James on Devstral unless hardware or the approved
   model policy changes.

Stop and roll back any future wave on wrong model identity, stale health,
insufficient context, memory pressure, or loss of the fallback coder pool.
