# Mission Control Legacy → ForgeFleet Migration

## Source: ~/taylorProjects/mission-control-legacy
## Target: ForgeFleet ff-mc crate + ff-gateway + dashboard

## Features to Migrate (Priority Order)

### HIGH PRIORITY
1. **Review Checklist System** — per-ticket review items with pass/skip/fail verdicts, review gating
2. **Work Item Events & History** — full audit trail of all state changes
3. **Docker Stacks** — per-ticket Docker isolation with compose files, volumes, ports
4. **Counsel Mode** — multi-model AI review (multiple LLMs review same work item)
5. **Task Groups** — sequential/parallel execution modes with sequence ordering
6. **PR/Branch Management** — branch_name, pr_number, pr_url, pr_status per work item
7. **Node Messages** — inter-node fleet messaging system

### MEDIUM PRIORITY
8. **Chat Sessions** — full conversation history with mode support
9. **Manual Timers** — per-work-item timer (start/pause/stop/reset)
10. **Escalation Workflow** — escalation_level tracking with auto-escalation
11. **Failed Models Tracking** �� retry_count, max_retries, failed_models per item
12. **Content Deduplication** — SHA256 hash prevents duplicate work items
13. **Redis Event Bus** — real-time pub/sub for fleet events (SSE endpoint)
14. **Model Performance Tracking** ��� quality scores per model per task
15. **Fleet Control** — pause/resume fleet, route work to specific nodes

### LOWER PRIORITY
16. **AI Hierarchy Generation** — prompt → auto-generate epic/feature/ticket tree
17. **GitHub Integration** — scan repos, manage collaborators, visibility toggle
18. **System Stats** — CPU, memory, disk, network monitoring
19. **Role-Model Matrix** — UI for mapping roles to preferred models

## Missing Database Tables
- `review_items` — per-ticket review checklist
- `work_item_events` — audit trail
- `docker_stacks` — container isolation
- `chat_sessions` — conversation history
- `node_messages` — fleet messaging
- `model_performance` — model quality tracking
- `work_item_dependencies` — dependency join table

## Missing Work Item Fields
- counsel_mode, counsel_models, counsel_responses, confidence, dissent
- escalation_level, escalation_reason
- manual_timer_state, manual_timer_started_at, manual_timer_paused_at, manual_timer_elapsed_ms
- retry_count, max_retries, failed_models
- content_hash, direct_assign
- branch_name, base_branch, pr_number, pr_url, pr_status, merged_at
- task_group, sequence_order
- review_bounce_count, rejection_history, rejection_round
- docker_stack_id
- builder_node_id, reviewer_node_id
- assignee, deadline, context
