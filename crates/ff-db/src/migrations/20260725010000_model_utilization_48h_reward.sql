CREATE OR REPLACE VIEW v_model_utilization AS
WITH interaction_stats AS (
    SELECT
        replace(substring(engine FROM 7), ' ', '-') AS model_id,
        COUNT(*) AS calls_7d,
        COALESCE(SUM(tokens_in), 0) + COALESCE(SUM(tokens_out), 0) AS tokens_7d
    FROM ff_interactions
    WHERE engine LIKE 'local:%'
      AND ts >= NOW() - INTERVAL '7 days'
    GROUP BY 1
),
build_stats AS (
    SELECT
        replace(substring(q.builder FROM 7), ' ', '-') AS model_id,
        COUNT(*) AS builds_7d,
        ROUND(
            100.0 * COUNT(*) FILTER (WHERE q.review_verdict = 'approve')
            / NULLIF(COUNT(*) FILTER (WHERE q.review_verdict IN ('approve', 'reject')), 0),
            1
        )::double precision AS approve_pct,
        COUNT(*) FILTER (
            WHERE q.enqueued_at >= NOW() - INTERVAL '48 hours'
        ) AS builds_48h,
        ROUND(
            100.0 * COUNT(*) FILTER (
                WHERE q.enqueued_at >= NOW() - INTERVAL '48 hours'
                  AND q.review_verdict = 'approve'
            )
            / NULLIF(COUNT(*) FILTER (
                WHERE q.enqueued_at >= NOW() - INTERVAL '48 hours'
                  AND q.review_verdict IN ('approve', 'reject')
            ), 0),
            1
        )::double precision AS approve_pct_48h
    FROM work_item_merge_queue q
    JOIN work_items w ON w.id = q.work_item_id
    WHERE q.builder LIKE 'local:%'
      AND q.enqueued_at >= NOW() - INTERVAL '7 days'
    GROUP BY 1
),
deployment_stats AS (
    SELECT
        catalog_id AS model_id,
        COUNT(*) AS instances,
        COALESCE(SUM(parallel_slots), 0) AS parallel_slots,
        MIN(context_window) AS context_window_min,
        MAX(context_window) AS context_window_max,
        MIN(usable_agent_ctx) AS usable_agent_ctx_min,
        MAX(usable_agent_ctx) AS usable_agent_ctx_max,
        BOOL_OR(usable_agent_ctx > context_window OR usable_agent_ctx < 8192) AS ctx_row_warn,
        COUNT(DISTINCT context_window) > 1 AS ctx_window_inconsistent
    FROM fleet_model_deployments
    WHERE catalog_id IS NOT NULL
    GROUP BY catalog_id
),
library_stats AS (
    SELECT
        catalog_id AS model_id,
        ROUND(SUM(size_bytes)::numeric / (1024 * 1024 * 1024), 2)::double precision AS est_ram_gb
    FROM fleet_model_library
    GROUP BY catalog_id
)
SELECT
    COALESCE(i.model_id, b.model_id, d.model_id, l.model_id) AS model_id,
    COALESCE(i.calls_7d, 0) AS calls_7d,
    COALESCE(i.tokens_7d, 0) AS tokens_7d,
    COALESCE(b.builds_7d, 0) AS builds_7d,
    b.approve_pct,
    COALESCE(d.instances, 0) AS instances,
    COALESCE(d.parallel_slots, 0) AS parallel_slots,
    d.context_window_min,
    d.context_window_max,
    d.usable_agent_ctx_min,
    d.usable_agent_ctx_max,
    l.est_ram_gb,
    COALESCE(d.ctx_row_warn, FALSE) OR COALESCE(d.ctx_window_inconsistent, FALSE) AS ctx_warn,
    CASE
        WHEN d.model_id IS NULL THEN NULL
        WHEN COALESCE(d.ctx_row_warn, FALSE) AND COALESCE(d.ctx_window_inconsistent, FALSE)
            THEN 'usable_agent_ctx out of bounds on at least one deployment AND context_window inconsistent across deployments'
        WHEN COALESCE(d.ctx_row_warn, FALSE)
            THEN 'usable_agent_ctx > context_window or < 8192 on at least one deployment'
        WHEN COALESCE(d.ctx_window_inconsistent, FALSE)
            THEN 'context_window inconsistent across deployments of this model'
        ELSE NULL
    END AS ctx_warn_reason,
    COALESCE(b.builds_48h, 0) AS builds_48h,
    b.approve_pct_48h
FROM interaction_stats i
FULL JOIN build_stats b ON b.model_id = i.model_id
FULL JOIN deployment_stats d ON d.model_id = COALESCE(i.model_id, b.model_id)
FULL JOIN library_stats l ON l.model_id = COALESCE(i.model_id, b.model_id, d.model_id)
ORDER BY model_id;
