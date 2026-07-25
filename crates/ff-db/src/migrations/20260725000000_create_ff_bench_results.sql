CREATE TABLE IF NOT EXISTS ff_bench_results (
 id UUID PRIMARY KEY DEFAULT gen_random_uuid(), model_id TEXT NOT NULL,
 suite TEXT NOT NULL DEFAULT 'forge-fleet-v1', suite_version INT NOT NULL DEFAULT 1,
 resolved_tasks INT NOT NULL CHECK(resolved_tasks>=0),
 total_tasks INT NOT NULL CHECK(total_tasks>0),
 resolve_rate DOUBLE PRECISION NOT NULL CHECK(resolve_rate BETWEEN 0 AND 1),
 task_results JSONB NOT NULL DEFAULT '[]', duration_ms BIGINT NOT NULL CHECK(duration_ms>=0),
 created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), CHECK(resolved_tasks<=total_tasks)
);
CREATE INDEX IF NOT EXISTS idx_ff_bench_results_model_created
 ON ff_bench_results(model_id,created_at DESC);
CREATE OR REPLACE VIEW v_model_utilization AS
WITH interaction_stats AS (
 SELECT replace(substring(engine FROM 7),' ','-') model_id,count(*) calls_7d,
 COALESCE(sum(tokens_in),0)+COALESCE(sum(tokens_out),0) tokens_7d
 FROM ff_interactions WHERE engine LIKE 'local:%' AND ts>=now()-interval '7 days' GROUP BY 1
), build_stats AS (
 SELECT replace(substring(q.builder FROM 7),' ','-') model_id,count(*) builds_7d,
 round(100.0*count(*) FILTER(WHERE q.review_verdict='approve')/
 NULLIF(count(*) FILTER(WHERE q.review_verdict IN('approve','reject')),0),1)::double precision approve_pct
 FROM work_item_merge_queue q JOIN work_items w ON w.id=q.work_item_id
 WHERE q.builder LIKE 'local:%' AND q.enqueued_at>=now()-interval '7 days' GROUP BY 1
), deployment_stats AS (
 SELECT catalog_id model_id,count(*) instances,COALESCE(sum(parallel_slots),0) parallel_slots,
 min(context_window) context_window_min,max(context_window) context_window_max,
 min(usable_agent_ctx) usable_agent_ctx_min,max(usable_agent_ctx) usable_agent_ctx_max,
 bool_or(usable_agent_ctx>context_window OR usable_agent_ctx<8192) ctx_row_warn,
 count(DISTINCT context_window)>1 ctx_window_inconsistent
 FROM fleet_model_deployments WHERE catalog_id IS NOT NULL GROUP BY catalog_id
), library_stats AS (
 SELECT catalog_id model_id,
 round(sum(size_bytes)::numeric/(1024*1024*1024),2)::double precision est_ram_gb
 FROM fleet_model_library GROUP BY catalog_id
), bench_stats AS (
 SELECT DISTINCT ON(model_id) model_id,resolve_rate bench_resolve_rate,created_at bench_scored_at
 FROM ff_bench_results WHERE suite='forge-fleet-v1' ORDER BY model_id,created_at DESC
)
SELECT COALESCE(i.model_id,b.model_id,d.model_id,l.model_id,x.model_id) model_id,
 COALESCE(i.calls_7d,0) calls_7d,COALESCE(i.tokens_7d,0) tokens_7d,
 COALESCE(b.builds_7d,0) builds_7d,b.approve_pct,COALESCE(d.instances,0) instances,
 COALESCE(d.parallel_slots,0) parallel_slots,d.context_window_min,d.context_window_max,
 d.usable_agent_ctx_min,d.usable_agent_ctx_max,l.est_ram_gb,
 COALESCE(d.ctx_row_warn,false) OR COALESCE(d.ctx_window_inconsistent,false) ctx_warn,
 CASE WHEN d.model_id IS NULL THEN NULL
 WHEN COALESCE(d.ctx_row_warn,false) AND COALESCE(d.ctx_window_inconsistent,false)
 THEN 'usable_agent_ctx out of bounds on at least one deployment AND context_window inconsistent across deployments'
 WHEN COALESCE(d.ctx_row_warn,false)
 THEN 'usable_agent_ctx > context_window or < 8192 on at least one deployment'
 WHEN COALESCE(d.ctx_window_inconsistent,false)
 THEN 'context_window inconsistent across deployments of this model' ELSE NULL END ctx_warn_reason,
 x.bench_resolve_rate,x.bench_resolve_rate capability_score,x.bench_scored_at
FROM interaction_stats i FULL JOIN build_stats b ON b.model_id=i.model_id
FULL JOIN deployment_stats d ON d.model_id=COALESCE(i.model_id,b.model_id)
FULL JOIN library_stats l ON l.model_id=COALESCE(i.model_id,b.model_id,d.model_id)
FULL JOIN bench_stats x ON x.model_id=COALESCE(i.model_id,b.model_id,d.model_id,l.model_id)
ORDER BY COALESCE(i.model_id,b.model_id,d.model_id,l.model_id,x.model_id);
