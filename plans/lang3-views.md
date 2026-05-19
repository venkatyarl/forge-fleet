# LANG.3 — FF-flavored Langfuse views

**Status:** ready to apply once the OTLP exporter is re-enabled (the
opentelemetry-otlp 0.27 batch exporter panics at daemon shutdown —
temporarily disabled by clearing `FORGEFLEET_OTEL_ENDPOINT` in the
Taylor launchd plist).

## What this is

Three saved views on top of Langfuse v3's Clickhouse trace store that
let the operator answer fleet-specific questions without writing
Clickhouse SQL by hand each time.

## Prerequisite — fix OTLP shutdown panic

```
thread 'main' panicked at tokio-1.51.0/.../shutdown.rs:51:21:
Cannot drop a runtime in a context where blocking is not allowed.
```

The fix is to call `opentelemetry::global::shutdown_tracer_provider()`
in a non-async context BEFORE the tokio runtime drops. In
`src/main.rs`, register a `tokio::task::spawn_blocking` shutdown hook
that fires before the runtime's drop.

Reference: <https://github.com/open-telemetry/opentelemetry-rust/issues/2068>

Once the daemon survives shutdown, re-enable OTLP by setting
`FORGEFLEET_OTEL_ENDPOINT=http://127.0.0.1:53000/api/public/otel/v1/traces`
back in the plist and `launchctl kickstart -k`.

## View 1 — spans by computer

```sql
-- runs against langfuse Clickhouse
CREATE OR REPLACE VIEW ff_spans_by_computer AS
SELECT
    JSONExtractString(metadata, 'ff.computer') AS computer,
    JSONExtractString(metadata, 'ff.role')     AS role,
    count()                                    AS span_count,
    sum(end_time - start_time)                 AS total_duration_ms,
    avg(end_time - start_time)                 AS avg_duration_ms,
    sum(JSONExtractInt(usage_details, 'input_tokens') +
        JSONExtractInt(usage_details, 'output_tokens')) AS total_tokens,
    max(start_time) AS last_seen
FROM observations
WHERE start_time > now() - INTERVAL 7 DAY
GROUP BY computer, role
ORDER BY total_duration_ms DESC;
```

Use: "which computer is doing the most LLM work this week"

## View 2 — cost split by tier

```sql
CREATE OR REPLACE VIEW ff_cost_split AS
SELECT
    JSONExtractString(metadata, 'ff.tier')     AS tier,         -- 9b/30b/72b/cloud
    JSONExtractString(metadata, 'ff.role')     AS role,         -- writer/judge/researcher/...
    count()                                    AS calls,
    sum(JSONExtractFloat(cost_details, 'total_cost')) AS total_cost_usd,
    avg(JSONExtractFloat(cost_details, 'total_cost')) AS avg_cost_per_call_usd,
    sum(JSONExtractInt(usage_details, 'output_tokens')) AS output_tokens,
    sum(JSONExtractInt(usage_details, 'input_tokens'))  AS input_tokens
FROM observations
WHERE start_time > now() - INTERVAL 30 DAY
  AND type = 'GENERATION'
GROUP BY tier, role
ORDER BY total_cost_usd DESC;
```

Use: "how much are we saving by routing to local? what % of work is
going to cloud and is that justified by the cost?"

## View 3 — fabric saturation

This one straddles two stores — Langfuse (per-span fabric attribute)
and ForgeFleet's `fabric_measurements` (actual TB / CX-7 bandwidth).
Materialize hourly from a defer-worker job that joins the two:

```sql
-- in ForgeFleet Postgres:
CREATE TABLE IF NOT EXISTS fabric_saturation_snapshots (
    snapshot_at  TIMESTAMPTZ NOT NULL,
    pair_name    TEXT        NOT NULL,
    spans_using  BIGINT      NOT NULL,
    bytes_pushed BIGINT      NOT NULL,
    measured_gbps DOUBLE PRECISION,
    saturation_pct DOUBLE PRECISION,  -- bytes_pushed / measured_gbps
    PRIMARY KEY (snapshot_at, pair_name)
);
```

Update on a 1-hour `ff defer-worker` job that:
1. Queries Clickhouse for spans in the last hour with `ff.fabric != ''`
2. Sums their `tokens * 4` (rough bytes estimate)
3. Joins with `fabric_pairs.measured_bandwidth_gbps`
4. Computes saturation %

Use: "is the CX-7 link fully utilized when TP=2 vLLM runs?"

## Wiring into ff

Add:
```
ff lang views        # show all three view names + last refresh
ff lang cost --since 30d
ff lang fabric --pair adele-sia
```

These are read-only — they just SELECT from the views above. No
mutation logic needed.
