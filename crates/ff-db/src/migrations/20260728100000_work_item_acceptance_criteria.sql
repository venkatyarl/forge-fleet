-- Acceptance criteria (the Anthropic long-running-agent "feature list" pattern):
-- a natural-language, checkable list of what "done RIGHT" means for a work_item.
-- Distinct from the existing SQL `acceptance_check` (machine existence assertion):
-- this is the human-readable checklist the DECOMPOSE PLANNER writes, the BUILDER
-- is given up front (constrains weaker/local models — the whole point), and the
-- SELF-VERIFY gate checks the produced diff against before a PR. JSON array of
-- {"criterion": "...", "kind": "mechanical|semantic"}.
ALTER TABLE work_items
    ADD COLUMN IF NOT EXISTS acceptance_criteria JSONB;
