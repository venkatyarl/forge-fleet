-- FF-flavored Langfuse Clickhouse views (LANG.3).
--
-- Re-applied automatically when the langfuse-clickhouse container is
-- rebuilt — the operator should mount this file at
-- /docker-entrypoint-initdb.d/ff_views.sql, OR run the file via
-- `docker exec -i forgefleet-langfuse-clickhouse clickhouse-client
--  --multiquery < deploy/langfuse/ff_views.sql` after compose up.
--
-- Why this lives in deploy/ instead of as a Postgres migration:
-- Langfuse v3 stores spans/traces in Clickhouse, not Postgres. Our
-- existing V-series migration framework targets ForgeFleet's
-- forgefleet DB.
--
-- Data shape we depend on:
--   metadata is Map(String, String) with one key 'resourceAttributes'
--   whose VALUE is a JSON string like
--   {"ff.computer":"taylor","service.name":"forgefleetd","ff.role":"daemon"}
--   so every read needs JSONExtractString(metadata['resourceAttributes'], …).
--
-- Verified on 2026-05-20 with 88 observations from forgefleetd@taylor.

DROP VIEW IF EXISTS default.ff_spans_by_computer;
DROP VIEW IF EXISTS default.ff_cost_split;
DROP VIEW IF EXISTS default.ff_recent_traces;

-- "Which computer is doing the most LLM work this week?"
CREATE VIEW default.ff_spans_by_computer AS
SELECT
    JSONExtractString(metadata['resourceAttributes'], 'ff.computer') AS computer,
    JSONExtractString(metadata['resourceAttributes'], 'ff.role')     AS role,
    count()                                                          AS span_count,
    sum(dateDiff('millisecond', start_time, end_time))               AS total_ms,
    avg(dateDiff('millisecond', start_time, end_time))               AS avg_ms,
    sum(usage_details['input_tokens'] + usage_details['output_tokens']) AS total_tokens,
    max(start_time)                                                  AS last_seen
FROM default.observations
WHERE start_time > now() - INTERVAL 7 DAY
  AND JSONExtractString(metadata['resourceAttributes'], 'ff.computer') != ''
GROUP BY computer, role
ORDER BY total_ms DESC;

-- "How much are we saving by routing to local? What % is going to
-- cloud and is that justified?"
CREATE VIEW default.ff_cost_split AS
SELECT
    JSONExtractString(metadata['resourceAttributes'], 'ff.tier') AS tier,
    JSONExtractString(metadata['resourceAttributes'], 'ff.role') AS role,
    count()                                  AS calls,
    sum(total_cost)                          AS total_cost_usd,
    avg(total_cost)                          AS avg_cost_per_call_usd,
    sum(usage_details['output_tokens'])      AS output_tokens,
    sum(usage_details['input_tokens'])       AS input_tokens
FROM default.observations
WHERE start_time > now() - INTERVAL 30 DAY
  AND type = 'GENERATION'
GROUP BY tier, role
ORDER BY total_cost_usd DESC;

-- "What did the fleet do in the last 24h?"
CREATE VIEW default.ff_recent_traces AS
SELECT
    t.id        AS trace_id,
    t.name      AS goal,
    t.timestamp AS started_at,
    JSONExtractString(t.metadata['resourceAttributes'], 'ff.computer') AS computer,
    JSONExtractString(t.metadata['resourceAttributes'], 'ff.role')     AS role,
    countIf(o.type = 'GENERATION')          AS llm_calls,
    sum(o.total_cost)                       AS total_cost_usd,
    sum(o.usage_details['input_tokens'] + o.usage_details['output_tokens']) AS total_tokens
FROM default.traces t
LEFT JOIN default.observations o ON o.trace_id = t.id
WHERE t.timestamp > now() - INTERVAL 24 HOUR
GROUP BY trace_id, goal, started_at, computer, role
ORDER BY started_at DESC
LIMIT 100;
