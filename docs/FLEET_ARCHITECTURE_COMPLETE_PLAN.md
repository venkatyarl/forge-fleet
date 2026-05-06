# ForgeFleet — Complete Architecture Plan

> **Single source of truth.** This document replaces `FLEET_ARCHITECTURE_RESEARCH.md`, `FLEET_ARCHITECTURE_MASTER_PLAN.md`, and all previous architecture notes. It contains: research findings, the full implementation roadmap, database schemas, component designs, and validation criteria.
>
> **Version**: 2026.5.5_3  
> **Fleet**: 6 nodes (Taylor leader + 5 followers: James, Marcus, Sophie, Priya, Ace)  
> **Database**: PostgreSQL at 192.168.5.100:55432 (postgres_full mode)  
> **Current SHA**: 457bbcc3c  
> **Principle**: **Fleet-first is the default.** Every task is distributed unless it explicitly requests local execution.

---

## Table of Contents

1. [Why This Document Exists](#1-why-this-document-exists)
2. [Core Philosophy: Fleet-First by Default](#2-core-philosophy-fleet-first-by-default)
3. [The Sub-Agent Workspace Model](#3-the-sub-agent-workspace-model)
4. [Work Queue Partitioning & Batch Splitting](#4-work-queue-partitioning--batch-splitting)
5. [Fleet Tool Registry & Usage Tracking](#5-fleet-tool-registry--usage-tracking)
6. [The 16-Phase Implementation Plan](#6-the-16-phase-implementation-plan)
7. [Database Schema](#7-database-schema)
8. [Component Architecture](#8-component-architecture)
9. [Validation Checklist](#9-validation-checklist)
10. [Risk Assessment & Mitigations](#10-risk-assessment--mitigations)
11. [Appendix: Research Sources](#11-appendix-research-sources)

---

## 1. Why This Document Exists

**Previously** we had two documents:
- `FLEET_ARCHITECTURE_RESEARCH.md` — raw findings from online research, protocol analysis, and framework comparisons. It was reference material.
- `FLEET_ARCHITECTURE_MASTER_PLAN.md` — the implementation roadmap with phases, schemas, and timelines.

**The problem**: Research findings and implementation plans were in separate files. When requirements evolved (like the fleet-first mandate and sub-agent workspace model), updates had to touch both. This caused drift and made it hard to see the complete picture.

**This document** merges everything into one canonical plan. Research findings are embedded as "Design Rationale" sections within each phase. Nothing is cut off. Everything is here.

---

## 2. Core Philosophy: Fleet-First by Default

### 2.1 The Old Way (Local-First)

```
User on Taylor: "Research X"
    │
    ▼
Taylor's AgentLoop runs everything
- LLM inference on Taylor
- Bash on Taylor
- Read/Write on Taylor
- Web search on Taylor
    │
    ▼
Result returned to user

Other 14 nodes: idle
```

### 2.2 The New Way (Fleet-First)

```
User on Taylor: "Research X"
    │
    ▼
Taylor's PlannerLLM decomposes into batches
    │
    ├─► Batch 1: web search → dispatched to Node 3
    ├─► Batch 2: web search → dispatched to Node 7
    ├─► Batch 3: deep analysis → dispatched to Node 11
    ├─► Batch 4: fact checking → dispatched to Node 14
    └─► Batch 5: synthesis → waits for 1-4
    │
    ▼
Each batch runs in a sub-agent workspace
on a DIFFERENT node (FleetFirst routing)
    │
    ▼
Results aggregated, final answer to user

All 6 nodes: actively participating
```

### 2.3 The Rules

| Rule | Description |
|------|-------------|
| **R1** | Every task is distributed **by default** unless marked `local_only` |
| **R2** | The local node gets a **selfish penalty** for background work — other nodes are preferred |
| **R3** | Complex tasks (est. >5 tool calls, or >1000 tokens, or explicit "research"/"analyze") are **automatically decomposed** |
| **R4** | Agents work in **hierarchical workspaces** — `~/.forgefleet/agents/agent-{n}/sub-agents/sub-agent-{m}/` |
| **R5** | Only **used artifacts** are promoted to the target location; leftovers are cleaned up |
| **R6** | The fleet decides work assignment, not individual computers — the scheduler partitions batches |
| **R7** | Tool usage is tracked fleet-wide for observability, but scheduling is by **node capacity**, not tool availability |

---

## 3. The Sub-Agent Workspace Model

### 3.1 Directory Structure

Every node has the same hierarchical structure:

```
~/.forgefleet/
├── agents/                           ← One per agent instance on this computer
│   └── agent-{id}/                   ← e.g. agent-taylor-0, agent-marcus-0
│       ├── config/
│       │   ├── agent.json            ← Agent identity, role, capabilities
│       │   ├── allowed_tools.json    ← Tool allow-list for this agent
│       │   └── env.sh                ← Environment variables
│       ├── memory/
│       │   ├── episodic/             ← Session histories
│       │   ├── semantic/             ← RAG chunks (local cache)
│       │   └── procedural/           ← Learned patterns (local cache)
│       ├── sessions/                 ← Active session metadata
│       ├── checkpoints/              ← Session state snapshots
│       ├── workspace/                ← Agent's own working directory
│       │   └── .git/                 ← Agent tracks its own changes
│       ├── sub-agents/               ← ← SUB-AGENTS LIVE HERE
│       │   ├── sub-agent-0/          ← Isolated sub-agent workspace
│       │   │   ├── work/             ← Working directory for tool execution
│       │   │   ├── repos/            ← Git clones (per-project)
│       │   │   ├── artifacts/
│       │   │   │   ├── pending/      ← Artifacts not yet promoted
│       │   │   │   └── promoted/     ← Artifacts copied to target (kept 7d)
│       │   │   ├── temp/             ← Temporary files (cleaned every run)
│       │   │   └── .metadata.json    ← Workspace state
│       │   ├── sub-agent-1/
│       │   ├── sub-agent-2/
│       │   └── sub-agent-3/
│       ├── logs/
│       └── .metadata.json            ← Agent-level metadata
├── shared-workspaces/                ← Shared fleet workspaces (synced)
│   └── {workspace_id}/
│       ├── git/
│       ├── nfs_mount/
│       └── s3_cache/
└── cleanup.log                       ← Cleanup cron activity log
```

**Hierarchy**: Fleet (6 nodes) → Computer (1 node) → **Agent** (1-N per node) → **Sub-Agent** (0-4 per agent)

**Why multiple agents per computer?**
- **Project isolation**: `agent-taylor-0` for Project A, `agent-taylor-1` for Project B
- **Role separation**: Research agent vs Coding agent vs Review agent
- **Security boundaries**: One agent has access to sensitive tools, another doesn't
- **Multi-tenancy**: Different users can have their own agents on the same node
- **Experimentation**: Test new configurations without affecting production agent

### 3.2 Workspace Lifecycle

```
1. CREATE (Agent level)
   Fleet scheduler assigns agent-{id} on Node X
   → Creates agent dir if not exists
   → Loads agent config (role, capabilities, tool allow-list)
   → Initializes memory systems

2. CREATE (Sub-agent level)
   Agent spawns sub-agent-{m} for parallel work
   → Creates sub-agent workspace dir
   → Clones git repo (if needed) into repos/{project}/
   → Sets working_dir = agents/agent-{id}/sub-agents/sub-agent-{m}/work/

3. EXECUTE
   Sub-agent runs tools in its isolated workspace
   → Bash commands run in work/
   → Files read from repos/{project}/
   → New artifacts written to artifacts/pending/
   → Agent monitors sub-agent progress, can yield/rebalance items

4. PROMOTE
   Sub-agent reports which artifacts were "used"
   → Promoter copies used artifacts to target location
   → Git changes: committed in sub-agent repo, then pushed/patched to origin
   → Images/videos: only used ones copied; unused stay in pending/

5. TEARDOWN (Sub-agent level)
   Sub-agent session ends
   → shell_state saved to Postgres
   → workspace marked idle
   → temp/ cleared immediately
   → Agent can respawn this sub-agent for new work

6. TEARDOWN (Agent level)
   Agent session ends
   → Memory checkpoints saved
   → Agent marked idle
   → Sub-agent workspaces preserved for potential reuse
```

### 3.3 Git Handling

**Each sub-agent gets its own git clone**:
```bash
# When sub-agent starts on a code task:
cd ~/.forgefleet/agents/agent-{id}/sub-agents/sub-agent-{m}/repos/
git clone --depth 1 {repo_url} {project_name}
cd {project_name}
git checkout -b subagent/{session_id}
# ... work happens ...
git add .
git diff > ~/.forgefleet/agents/agent-{id}/sub-agents/sub-agent-{m}/artifacts/pending/changes.patch
```

**Promotion options**:
- **Patch mode** (default): Generate a patch, apply it to the main repo on the requestor's node
- **Push mode**: Sub-agent pushes its branch; requestor merges it
- **Direct mode** (local-only tasks): Write directly to the shared workspace

### 3.4 Artifact Promotion Rules

| Artifact Type | Promotion Rule | Destination |
|---------------|----------------|-------------|
| **Document** (`.md`, `.txt`, `.pdf`) | Promoted if referenced in final response | Target path from user intent |
| **Image** (`.png`, `.jpg`, `.svg`) | Promoted if used in output | `assets/` or specified path |
| **Video** (`.mp4`, `.mov`) | Promoted if referenced | `assets/videos/` or specified path |
| **Code file** | Promoted if part of the solution | Repo working tree |
| **Data file** (`.json`, `.csv`) | Promoted if used downstream | Target path or `data/` |
| **Temporary** (`.tmp`, cache) | Never promoted | Deleted on teardown |

**The sub-agent decides what to promote** by returning `promoted_artifacts: Vec<ArtifactRef>` in its final result. The parent agent's promoter component handles the actual copy.

### 3.5 Cleanup Policy

A daily cron job (`ff-cron` or `fleet_tasks` scheduled task) runs cleanup:

```sql
-- Cleanup tracking table
CREATE TABLE subagent_cleanup_log (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    node_id UUID REFERENCES fleet_nodes(id),
    subagent_id TEXT NOT NULL,
    item_type TEXT NOT NULL CHECK (item_type IN ('git_folder','artifact','temp','empty_dir')),
    item_path TEXT NOT NULL,
    bytes_freed BIGINT,
    reason TEXT NOT NULL,
    deleted_at TIMESTAMPTZ DEFAULT NOW()
);
```

**Cleanup rules** (executed in order):

1. **Temp folders** — Delete everything in `temp/` (every run)
2. **Promoted artifacts** — Delete from `artifacts/promoted/` after 7 days
3. **Pending artifacts** — Delete from `artifacts/pending/` after **30 days** if never promoted
4. **Git folders** — Delete from `repos/` after **60 days** of no activity (no commits, no reads)
5. **Empty directories** — Remove empty sub-agent folders

**Safety**: A git folder is only deleted if:
- No uncommitted changes
- No unpushed commits (or branch already merged)
- Last access > 60 days
- Not currently locked by a running sub-agent
- Agent owning the sub-agent is not active

**NATS event** on cleanup:
```json
{
  "subject": "fleet.cleanup.completed",
  "node_id": "...",
  "items_deleted": 47,
  "bytes_freed": 2147483648,
  "git_folders_removed": 3,
  "artifacts_removed": 12
}
```

---

## 4. Adaptive Work Distribution & Yield Protocol

### 4.1 The Problem with Fixed Batches

> *"What if an agent picking up the first 20 files is done because there is only 1 page in each of those files and then other agents have pages that are 40 or more? In that case the first agent and sub-agents don't have anything to do while the documents just sit until the other sub-agents are done working."*

**Fixed-size batches are naive.** They assume all items require equal effort. They don't.

**The solution**: Estimate item complexity, weight the partition, and enable **fine-grained work stealing with yield protocol**.

---

### 4.2 The Five-Phase Adaptive Distribution

```
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│  PHASE 0: SCAN  │───▶│ PHASE 1: WEIGHT │───▶│ PHASE 2: PARTITION│
│  (fast metadata)│    │  (estimate cost)│    │  (balanced bins)  │
└─────────────────┘    └─────────────────┘    └─────────────────┘
         │                                            │
         ▼                                            ▼
   Read first 4KB or         weight = f(size,          Greedy bin-packing:
   use file metadata         complexity, type)         heaviest first →
                                                      lowest-weight bin
                                                       │
    ┌─────────────────┐    ┌─────────────────┐        │
    │ PHASE 4: REBALANCE│◀──│  PHASE 3: EXECUTE │◀─────┘
    │  (yield + steal)  │    │  (claim + process)│
    └─────────────────┘    └─────────────────┘
             │
             ▼
   Fast worker finishes →
   steals remaining items
   from slow worker
```

---

### 4.3 Step 0: Pre-Scan (Fast Metadata Pass)

Before partitioning, the coordinator does a **quick metadata scan** of all items. No LLM calls — just filesystem stats, PDF page counts, URL HEAD requests, or code line counts.

```rust
pub struct ItemEstimator;

impl ItemEstimator {
    pub async fn estimate(item: &WorkItem) -> ItemWeight {
        match item.item_type {
            "document" => {
                let pages = pdf_page_count(&item.item_key).await.unwrap_or(1);
                let words = word_count(&item.item_key).await.unwrap_or(0);
                ItemWeight {
                    base: 1.0,
                    pages: pages as f64 * 2.0,      // 2 weight units per page
                    words: words as f64 * 0.001,    // 1 unit per 1000 words
                    has_images: has_images(&item.item_key) as i32 * 5.0,
                    has_code: has_code_blocks(&item.item_key) as i32 * 3.0,
                }
            }
            "url" => {
                let resp = reqwest::head(&item.item_key).await.ok();
                let content_length = resp.and_then(|r| r.content_length()).unwrap_or(0);
                ItemWeight {
                    base: 1.0 + content_length as f64 / 100000.0,  // 1 unit per 100KB
                    ..Default::default()
                }
            }
            "code_file" => {
                let lines = line_count(&item.item_key).await.unwrap_or(0);
                ItemWeight {
                    base: 1.0,
                    words: lines as f64 * 0.05,     // 1 unit per 20 lines
                    has_code: 1.0,
                    ..Default::default()
                }
            }
            _ => ItemWeight { base: 1.0, ..Default::default() },
        }
    }
}
```

**Estimated weight formula**:
```
total_weight = base + pages + words + has_images + has_code
```

Example for 200 documents:
| Doc | Pages | Words | Has Images | Has Code | Weight |
|-----|-------|-------|------------|----------|--------|
| doc_001.pdf | 1 | 500 | No | No | 1.5 |
| doc_042.pdf | 40 | 12000 | Yes | Yes | 101.0 |
| doc_100.pdf | 5 | 2000 | No | Yes | 15.0 |

---

### 4.4 Phase 1: Weighted Partitioning

Instead of `batch_size = 20`, we use **greedy bin-packing** to create `N` batches (where `N` = number of available workers) with roughly equal **total weight**.

```rust
fn weighted_partition(items: &mut [WorkItem], num_batches: usize) -> Vec<Vec<WorkItem>> {
    // Sort by weight descending (heaviest first — critical for bin-packing quality)
    items.sort_by(|a, b| b.estimated_weight.partial_cmp(&a.estimated_weight).unwrap());
    
    let mut batches: Vec<Vec<WorkItem>> = (0..num_batches).map(|_| Vec::new()).collect();
    let mut batch_weights: Vec<f64> = vec![0.0; num_batches];
    
    for item in items {
        // Assign to batch with lowest current weight
        let min_idx = batch_weights.iter().enumerate()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(idx, _)| idx)
            .unwrap();
        
        batch_weights[min_idx] += item.estimated_weight;
        batches[min_idx].push(item.clone());
    }
    
    batches
}
```

**Result**: Instead of 20 docs per batch, you might get:
- Batch 0: 45 lightweight docs (total weight ≈ 100)
- Batch 1: 3 heavyweight docs (total weight ≈ 100)
- Batch 2: 12 medium docs (total weight ≈ 100)

All batches have roughly equal **estimated effort**, not equal **item count**.

---

### 4.5 Phase 2: Claim & Execute

```
User: "Read these 200 documents and summarize"
    │
    ▼
PlannerLLM on Taylor (coordinator):
  strategy = "map_reduce"
  num_workers = 10  (based on fleet idle capacity)
    │
    ▼
SessionRunner:
  1. Scans all 200 docs → estimates weights
  2. Creates 10 weighted batches via greedy bin-packing
  3. Creates 200 work_items rows
  4. Creates 10 work_batches rows
    │
    ┌──────────┬──────────┬──────────┬──────────┬──────────┐
    ▼          ▼          ▼          ▼          ▼
 Batch 0    Batch 1    Batch 2    Batch 3    Batch 9
 45 docs    3 docs     12 docs    8 docs     22 docs
 weight≈100 weight≈100 weight≈100 weight≈100 weight≈100
    │          │          │          │          │
    ▼          ▼          ▼          ▼          ▼
 Node 3    Node 7     Node 11    Node 14    Node 2
(Agent-0)  (Agent-0)  (Agent-0)  (Agent-0)  (Agent-0)
    │          │          │          │          │
    ▼          ▼          ▼          ▼          ▼
Sub-agent  Sub-agent  Sub-agent  Sub-agent  Sub-agent
reads 45   reads 3    reads 12   reads 8    reads 22
light docs heavy docs med docs   med docs   light docs
    │          │          │          │          │
    └──────────┴──────────┴──────────┴──────────┘
                    │
                    ▼
            Coordinator aggregates
            final summary → user
```

---

### 4.6 Phase 3: Fine-Grained Work Stealing

Even with weighted partitioning, estimates can be wrong. A document might be unexpectedly complex (dense academic paper, corrupted PDF requiring OCR fallback).

**The yield protocol** allows a slow worker to release unfinished items:

```rust
// Slow worker realizes it's falling behind
async fn yield_item(&self, item_id: Uuid) -> Result<(), Error> {
    // 1. Save checkpoint (what was already done)
    let checkpoint = json!({
        "pages_processed": 15,
        "partial_summary": "So far we have learned...",
        "tool_calls_made": [...]
    });
    
    sqlx::query!(
        r#"
        UPDATE work_items
        SET status = 'pending',
            checkpoint_data = $1,
            yielded_at = NOW(),
            assigned_node_id = NULL,
            assigned_session_id = NULL
        WHERE id = $2
        "#,
        checkpoint, item_id
    ).execute(&self.pg).await?;
    
    Ok(())
}

// Fast worker claims the yielded item and resumes from checkpoint
async fn claim_with_checkpoint(&self, item_id: Uuid) -> Result<WorkItem, Error> {
    let item = sqlx::query!(
        r#"
        UPDATE work_items
        SET status = 'claimed',
            assigned_node_id = $1,
            assigned_session_id = $2,
            claimed_at = NOW(),
            stolen_from = assigned_node_id
        WHERE id = $3
          AND status = 'pending'
        RETURNING *
        "#,
        self.my_node_id, self.my_session_id, item_id
    ).fetch_one(&self.pg).await?;
    
    // Resume from checkpoint instead of starting from scratch
    if let Some(checkpoint) = item.checkpoint_data {
        self.resume_from_checkpoint(item_id, checkpoint).await?;
    }
    
    Ok(item)
}
```

**Rebalance trigger** (checked every 30 seconds):
```rust
fn should_rebalance(batch_progress: &[BatchProgress]) -> Vec<RebalanceOp> {
    let avg_progress = batch_progress.iter().map(|b| b.percent).sum::<f64>() 
                       / batch_progress.len() as f64;
    
    let mut ops = Vec::new();
    
    for batch in batch_progress {
        if batch.percent < avg_progress * 0.5 {
            // This batch is less than half the average progress
            // Find its remaining pending items and migrate them
            let remaining = batch.remaining_items.clone();
            let target = find_fastest_worker(batch_progress);
            ops.push(RebalanceOp {
                items: remaining,
                from: batch.node_id,
                to: target.node_id,
            });
        }
    }
    
    ops
}
```

**Work stealing at item level**:
- Fast worker finishes its batch → looks for **individual unclaimed items** in other batches
- If no unclaimed items → looks for **in-progress items that were yielded**
- Claims them, resumes from checkpoint, completes them
- Original worker never touches them again

---

### 4.7 Phase 4: Progress Reporting & Rebalancing

Every sub-agent reports progress every 30 seconds:

```json
{
  "subject": "fleet.work.progress",
  "batch_id": "...",
  "node_id": "...",
  "agent_id": "agent-taylor-0",
  "subagent_id": "sub-agent-2",
  "items_total": 45,
  "items_completed": 38,
  "items_in_progress": 2,
  "items_pending": 5,
  "current_item": "doc_037.pdf",
  "current_item_progress": 65,
  "estimated_remaining_sec": 120
}
```

The coordinator monitors all batches. If one batch is >2× slower than the average:
1. Coordinator sends `rebalance` signal to the slow batch
2. Slow worker yields its remaining items (saves checkpoints)
3. Idle or fast workers claim the yielded items
4. Work continues without waiting for the slow worker

---

### 4.8 The Work Items Table (Adaptive Version)

```sql
CREATE TABLE work_items (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    parent_task_id UUID NOT NULL REFERENCES fleet_tasks(id) ON DELETE CASCADE,
    batch_id INT NOT NULL,
    item_index INT NOT NULL,
    item_key TEXT NOT NULL,
    item_type TEXT NOT NULL,
    item_metadata JSONB DEFAULT '{}',

    -- Weighted estimation
    estimated_weight FLOAT NOT NULL DEFAULT 1.0,
    actual_weight FLOAT,               -- filled in after completion
    complexity_factors JSONB DEFAULT '{}',  -- {pages: 40, words: 12000, has_images: true}

    -- Assignment
    assigned_node_id UUID REFERENCES fleet_nodes(id),
    assigned_agent_id TEXT,            -- e.g. "agent-taylor-0"
    assigned_session_id UUID REFERENCES agent_sessions(id),
    claimed_at TIMESTAMPTZ,

    -- Progress
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'claimed', 'in_progress', 'completed', 'failed', 'yielded', 'stolen')),
    progress_percent INT DEFAULT 0,    -- 0-100, updated during execution
    checkpoint_data JSONB DEFAULT '{}', -- saved state for yield/resume
    yielded_at TIMESTAMPTZ,
    stolen_from UUID REFERENCES fleet_nodes(id),

    -- Result
    result_summary TEXT,
    result_artifact_id UUID,
    result_tokens_in INT DEFAULT 0,
    result_tokens_out INT DEFAULT 0,
    completed_at TIMESTAMPTZ,
    error_message TEXT,

    -- Retry
    retry_count INT DEFAULT 0,
    max_retries INT DEFAULT 2,

    created_at TIMESTAMPTZ DEFAULT NOW(),

    UNIQUE(parent_task_id, item_index)
);

CREATE INDEX idx_work_items_parent ON work_items(parent_task_id, status);
CREATE INDEX idx_work_items_batch ON work_items(parent_task_id, batch_id, status);
CREATE INDEX idx_work_items_claimed ON work_items(assigned_node_id, status)
    WHERE status IN ('claimed', 'in_progress');
CREATE INDEX idx_work_items_yielded ON work_items(parent_task_id, status)
    WHERE status = 'yielded';
```

---

### 4.9 The Work Batches Table

```sql
CREATE TABLE work_batches (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    parent_task_id UUID NOT NULL REFERENCES fleet_tasks(id) ON DELETE CASCADE,
    batch_index INT NOT NULL,
    total_estimated_weight FLOAT NOT NULL DEFAULT 0,
    total_actual_weight FLOAT,
    items_count INT NOT NULL,
    assigned_node_id UUID REFERENCES fleet_nodes(id),
    assigned_agent_id TEXT,
    assigned_session_id UUID REFERENCES agent_sessions(id),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'claimed', 'in_progress', 'completed', 'rebalancing')),
    progress_percent INT DEFAULT 0,
    rebalanced_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(parent_task_id, batch_index)
);
```

---

### 4.10 Map-Reduce Strategies

The PlannerLLM selects a strategy based on the task:

| Strategy | Use When | Partition Method | Rebalance? |
|----------|----------|------------------|------------|
| **map_reduce** | Independent items (documents, URLs) | Weighted bin-packing | Yes — item-level steal |
| **pipeline** | Sequential stages (build → test → deploy) | Stage-per-batch | No — stages are ordered |
| **vote** | Need consensus (code review, fact check) | Same task × N agents | No — all must complete |
| **competitive** | Best-result-wins (creative writing) | Same task × N agents | No — pick best result |
| **fanout_gather** | Heterogeneous specialists | Specialist-per-batch | Limited — within specialty |

---

## 5. Fleet Tool Registry & Usage Tracking

### 5.1 The Tool Reality

> *"Tools aren't local, they are fleet-wide, even though they exist on all the computers, since we are using ff and the tools are created by ff, most probably all the computers have the same tools."*

**Correct.** All 6 nodes run the same `ff-agent` binary with the same 52+ tools. So tool availability is not the scheduling constraint — **node capacity** is.

However, we still need to track tool usage for:
1. **Observability** — Which tools are hot? Which nodes are overloaded?
2. **Cost attribution** — Who spent tokens on what?
3. **Debugging** — Which tool failed on which node?
4. **Optimization** — Should we pre-load certain models based on tool usage patterns?

### 5.2 Fleet Tool Registry

```sql
CREATE TABLE fleet_tools (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_name TEXT NOT NULL,
    node_id UUID REFERENCES fleet_nodes(id) ON DELETE CASCADE,
    description TEXT NOT NULL,
    parameters_schema JSONB NOT NULL,
    capabilities_required TEXT[] DEFAULT '{}',
    health_checked_at TIMESTAMPTZ DEFAULT NOW(),
    call_count INT DEFAULT 0,
    avg_latency_ms FLOAT,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(tool_name, node_id)
);

CREATE INDEX idx_fleet_tools_name ON fleet_tools(tool_name);
CREATE INDEX idx_fleet_tools_node_health ON fleet_tools(node_id, health_checked_at);
```

**Registration**: Every node registers its tools on startup. Since all nodes have the same tools, the table will have 52 tools × 6 nodes = 312 rows. That's fine — it's indexed.

**Health pruning**: Tools not heartbeated in 5 minutes are marked unavailable (node might be down).

### 5.3 Tool Usage Tracking

```sql
CREATE TABLE fleet_tool_usage (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_name TEXT NOT NULL,
    node_id UUID REFERENCES fleet_nodes(id),
    session_id UUID REFERENCES agent_sessions(id),
    task_id UUID REFERENCES fleet_tasks(id),
    work_item_id UUID REFERENCES work_items(id),
    subagent_id TEXT,                  -- e.g. "sub-agent-2"
    input_summary TEXT,                -- truncated for audit (first 200 chars)
    started_at TIMESTAMPTZ DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    latency_ms INT,
    success BOOLEAN,
    tokens_in INT DEFAULT 0,
    tokens_out INT DEFAULT 0,
    cost_usd FLOAT DEFAULT 0.0,
    workspace_path TEXT                -- which sub-agent workspace was used
);

CREATE INDEX idx_tool_usage_tool ON fleet_tool_usage(tool_name, started_at);
CREATE INDEX idx_tool_usage_node ON fleet_tool_usage(node_id, started_at);
CREATE INDEX idx_tool_usage_session ON fleet_tool_usage(session_id);
```

**Example query**: "Show me all `Read` tool calls in the last hour, grouped by node"
```sql
SELECT node_id, COUNT(*) as calls, AVG(latency_ms) as avg_latency
FROM fleet_tool_usage
WHERE tool_name = 'Read'
  AND started_at > NOW() - INTERVAL '1 hour'
GROUP BY node_id
ORDER BY calls DESC;
```

### 5.4 The Scheduler Doesn't Schedule by Tool

**Important distinction**:
- The `fleet_tools` table answers: "Can any node run the Bash tool?" (Yes, all of them.)
- The scheduler answers: "Which node should run this Bash task?" (The one with lowest load, considering selfish penalty.)

The scheduler uses **node capacity metrics**, not tool presence:
```rust
struct NodeCapacity {
    node_id: Uuid,
    cpu_percent: f64,
    memory_percent: f64,
    gpu_utilization: f64,
    gpu_vram_used: f64,
    queue_depth: usize,
    active_sessions: usize,
    last_heartbeat: DateTime<Utc>,
}
```

### 5.5 Fleet Software Registry

**Track all software installed across the fleet.** Graphify is the first entry. Every tool that gets installed fleet-wide goes here.

```sql
CREATE TABLE fleet_software (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_name TEXT NOT NULL,
    version TEXT NOT NULL,
    install_method TEXT NOT NULL,       -- 'uv', 'pip', 'cargo', 'brew', 'apt'
    install_command TEXT NOT NULL,
    update_command TEXT,
    required_by TEXT[],                 -- which features need this
    installed_on TEXT[],                -- which nodes have it
    install_log JSONB DEFAULT '{}',     -- per-node install status
    last_checked_at TIMESTAMPTZ DEFAULT NOW(),
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

**Initial entries**:

| Tool | Version | Install | Required By | Status |
|------|---------|---------|-------------|--------|
| **graphify** | latest | `uv tool install graphifyy` | Phase 0, 16m | ✅ Taylor, ❌ Marcus, ❌ Sophie, ❌ Ace, ❌ James |
| **uv** | latest | `curl -LsSf https://astral.sh/uv/install.sh \| sh` | graphify, Python tooling | ✅ All nodes |
| **rust** | stable | `rustup` | ff-agent, ff-graph | ✅ All nodes |
| **postgres** | 16 | `brew/apt` | Primary DB | ✅ Taylor, ✅ Marcus |
| **redis** | 7 | `brew/apt` | Pulse metrics | ✅ Taylor |

**Fleet-wide installation workflow**:
```rust
// 1. Taylor (leader) installs first
async fn install_on_leader(tool: &FleetSoftware) -> InstallResult {
    let output = Command::new("sh").arg("-c").arg(&tool.install_command).output()?;
    verify_installation(tool).await
}

// 2. Distribute to all nodes via fleet deployment
async fn distribute_to_fleet(tool: &FleetSoftware) {
    for node in fleet_nodes().await? {
        if node.hostname == "taylor" { continue; } // already installed
        
        // SSH to node and run install command
        let result = ssh_exec(&node, &tool.install_command).await;
        
        // Update fleet_software table
        update_install_log(tool.id, &node.hostname, result).await?;
    }
}

// 3. Health check: verify all nodes have required tools
async fn software_health_check() -> HealthReport {
    let mut report = HealthReport::new();
    for tool in fleet_software().await? {
        for node in fleet_nodes().await? {
            let has_it = verify_on_node(&tool, &node).await;
            report.add(node.hostname, tool.tool_name, has_it);
        }
    }
    report
}
```

---

## 6. Fleet Memory Architecture — Full Vault Access Model

> **The fundamental shift**: ForgeFleet has **full read/write access** to the entire `~/projects/Yarli_KnowledgeBase` vault. There are no artificial boundaries. The vault is a living workspace that both you and ForgeFleet actively maintain.
>
> **Key principles**:
> 1. **Full access** — FF can read, write, delete, move, and merge any file in the vault
> 2. **Attribution** — Every FF edit is tracked via YAML frontmatter and git history
> 3. **Index-first** — FF reads `ForgeFleet/Index.md` first to orient itself, then follows links
> 4. **Daily cadence** — FF appends its activity to `Daily Notes/`
> 5. **TODO-aware** — FF scans, creates, and completes TODOs across the entire vault

### 6.1 The Three-Store Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    UNIFIED MEMORY — THREE-STORE ARCHITECTURE                 │
│                                                                              │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │  STORE 1: YARLI_KNOWLEDGEBASE (Source of Truth)                     │    │
│  │  ~/projects/Yarli_KnowledgeBase  — Git repo, Obsidian vault         │    │
│  │                                                                      │    │
│  │  graphify/                     ← VAULT CONCEPT GRAPH (root level)  │    │
│  │  │   ├── GRAPH_REPORT.md       ← God nodes, connections            │    │
│  │  │   └── graph.json            ← Queryable graph data               │    │
│  │                                                                      │    │
│  │  ForgeFleet/                    ← FF-managed + auto-generated       │    │
│  │  ├── Index.md                   ← MASTER INDEX (read first)        │    │
│  │  ├── Manifest.md                ← Auto-generated vault map          │    │
│  │  ├── Hive Mind/                                                     │    │
│  │  ├── Brain/                                                         │    │
│  │  ├── Computers/               ← Fleet node info (6 nodes)        │    │
│  │  ├── Projects/                                                      │    │
│  │  │   └── forge-fleet/                                              │    │
│  │  └── Agents/                                                        │    │
│  │                                                                      │    │
│  │  (User's knowledge — FF reads/writes here too)                     │    │
│  │  Daily Notes/                 ← FF appends daily activity          │    │
│  │  Projects/                    ← User projects (FF can modify)      │    │
│  │  Yarlagadda Home/            ← Personal (FF reads for context)     │    │
│  │  Electronics/Computers/      ← Hardware inventory (FF reads)       │    │
│  │  People/, Knowledge/, ...                                          │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                              ▲                                               │
│                              │ Git push/pull (auto-managed by ForgeFleet)   │
│                              ▼                                               │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │  STORE 2: ~/.forgefleet/memory/ (Local Working Cache)               │    │
│  │                                                                      │    │
│  │  ├── vault_mirror/      ← Full vault copy (fast local access)      │    │
│  │  ├── ephemeral/         ← plan.md, scratch notes (deleted after)   │    │
│  │  └── agents/{agent}/    ← Agent-specific working state             │    │
│  │       ├── compacted/    ← Persisted session summaries              │    │
│  │       └── logs/         ← Agent process logs                       │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                              ▲                                               │
│                              │ Sync daemon watches for changes              │
│                              ▼                                               │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │  STORE 3: POSTGRES (Search Index + Cache)                           │    │
│  │                                                                      │    │
│  │  • Full-text search (FTS5) across ALL vault markdown               │    │
│  │  • Vector embeddings for semantic search                           │    │
│  │  • Wiki-link graph as queryable edges table                        │    │
│  │  • TODO extraction: open/closed tasks across vault                 │    │
│  │  • Incremental sync: only changed files re-indexed                 │    │
│  │  • Index.md content cached for sub-second entry                    │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 6.2 Why Full Vault Access?

| Question | Answer |
|----------|--------|
| **Why give ForgeFleet full access?** | The vault is YOUR knowledge base, not a silo. FF should organize your Projects/, update your Daily Notes/, and even move files for better structure — just like a research assistant would. |
| **What if FF messes up my notes?** | Every change is git-tracked. Revert any commit. Plus FF adds `last_modified_by: ff-{agent}` to frontmatter so you know what changed. |
| **Why not keep FF in its own folder?** | We DO keep `ForgeFleet/` as the "home base" for generated memory. But FF can write anywhere when you ask it to. The boundary is soft, not hard. |
| **Can I still use Obsidian normally?** | Yes. FF works in the background. You edit normally. If you both touch the same file, conflict resolution kicks in. |
| **Why an Index.md?** | Instead of scanning 1000+ files every time, FF reads `ForgeFleet/Index.md` first (~5KB). This gives it the "lay of the land" — active projects, recent changes, open tasks — in milliseconds. |

### 6.3 Directory Structure — Current Vault + FF Additions

> **Status**: Numbered restructure (00-99) is **deferred**. The vault keeps its current structure. FF adds `ForgeFleet/` and `graphify/` without reorganizing existing user content.

**Design philosophy**: FF respects the existing vault. It reads and writes anywhere. `ForgeFleet/` is the system layer. `graphify/` at root is the vault's concept map.

```
~/projects/Yarli_KnowledgeBase/
│
├── .graphifyignore              ← What graphify skips (minimal!)
│
├── graphify/                    ← VAULT-WIDE CONCEPT GRAPH (auto-managed)
│   ├── graph.json               ← Queryable graph data
│   ├── GRAPH_REPORT.md          ← God nodes, connections, questions
│   └── manifest.json            ← File index
│
├── ForgeFleet/                  ← FLEET SYSTEM MEMORY (FF-managed, NEW)
│   ├── Index.md                 ← MASTER INDEX — read first!
│   ├── Manifest.md              ← Auto-generated vault statistics
│   ├── Hive Mind/               ← Fleet-wide standards
│   ├── Brain/                   ← User preferences & patterns
│   ├── Computers/               ← Fleet node info (6 nodes)
│   │   ├── taylor.md
│   │   ├── marcus.md
│   │   ├── sophie.md
│   │   ├── james.md
│   │   ├── priya.md
│   │   └── ace.md
│   ├── Projects/                ← FF project memory + code graphs
│   │   └── forge-fleet/
│   │       ├── Overview.md
│   │       ├── Architecture.md
│   │       ├── Decisions.md
│   │       ├── Active Work.md
│   │       └── graphify-codebase/
│   │           ├── graph.json
│   │           ├── GRAPH_REPORT.md
│   │           └── graph.html
│   ├── Agents/                  ← Per-agent memory
│   └── Templates/               ← FF-specific templates (see §6.3.1)
│
├── Daily Notes/                 ← DAILY ACTIVITY LOGS (existing)
│   └── YYYY/YYYY-MM/YYYY-MM-DD.md
│
├── Inbox/                       ← EXISTING USER FOLDERS (unchanged)
├── Business/
├── Career/
├── Kids/
├── Fitness/
├── Vehicles/
├── Knowledge/
├── People/
├── Electronics/
├── Recipes/
├── Random/
├── Yarlagadda Home/
│
├── templates/                   ← OBSIDIAN TEMPLATES (existing)
└── (other user folders...)
```

**FF respects existing structure**: FF reads and writes anywhere in the vault. It does not reorganize user folders without explicit instruction.

**Future restructure** (deferred to post-Phase 16):
- If numbered folders (00-99) are desired later, FF will perform the migration atomically
- All wiki-links `[[...]]` will be auto-updated during migration
- Rollback = `git revert` (single atomic commit)

#### 6.3.1 ForgeFleet Templates

FF creates pages using **two template sources**:

1. **ForgeFleet Templates** (for FF system pages):
   - `ForgeFleet/Templates/Index.md` — New Index entries
   - `ForgeFleet/Templates/Computer.md` — New node markdown
   - `ForgeFleet/Templates/Project.md` — New project overview
   - `ForgeFleet/Templates/Daily Note.md` — Daily activity log
   - `ForgeFleet/Templates/Decision.md` — Architecture decision record
   - `ForgeFleet/Templates/Agent Memory.md` — Per-agent learnings

2. **Existing Vault Templates** (for user-facing pages):
   - Uses templates from `templates/` or `ForgeFleet Templates/` (user's existing template folders)
   - Examples: Business research, TODO lists, meeting notes, project plans
   - FF detects template type from context: "research business" → business template, "todo list" → task template

**Template selection logic**:
```
if page_path.starts_with("ForgeFleet/"):
    use ForgeFleet/Templates/{type}.md
else:
    use existing vault template matching intent
    (fallback: ForgeFleet/Templates/Generic.md)
```

**Template variables** (YAML frontmatter + body):
- `{{date}}` → YYYY-MM-DD
- `{{time}}` → HH:MM
- `{{node}}` → current fleet node name
- `{{agent}}` → current agent ID
- `{{project}}` → inferred project name from context

### 6.4 Graphify — Build vs. Buy Decision

**Decision: Use graphify NOW. Build `ff-graph` (Rust) in parallel. Switch when ff-graph achieves parity.**

| Phase | Approach | Timeline |
|-------|----------|----------|
| **Now** | `graphify` subprocess from `ff-memory` | Weeks 1-4 |
| **Parallel** | Build `ff-graph` crate in Rust | Weeks 4-16 |
| **Switch** | Replace graphify with ff-graph | When ff-graph ≥ graphify features |

**Why not just use graphify forever?**

| Limitation | Why It Matters for ForgeFleet |
|------------|------------------------------|
| **Python dependency** | Fleet nodes run Rust binaries. Adding Python + pip is extra ops burden. |
| **No Postgres integration** | graphify doesn't know about `vault_files`, `vault_links`, `vault_todos`. ff-graph will. |
| **No fleet sync** | graphify is single-node. ff-graph will sync graph updates across 6 nodes via NATS. |
| **Generic folder processor** | graphify doesn't understand Obsidian frontmatter, Daily Notes hierarchy, or `[[wiki-links]]`. ff-graph will. |
| **Batch processing only** | graphify runs on command. ff-graph will update incrementally in real-time. |
| **No Obsidian-specific features** | ff-graph will leverage Dataview queries, tag inheritance, and folder-note relationships. |

**Why start with graphify?**
- Immediate value (today, not in 3 months)
- Proven at 42k+ stars, handles 28 languages
- Outputs perfect formats (`GRAPH_REPORT.md`, `graph.json`)
- We learn from it while building ff-graph

**ff-graph (Rust) target architecture**:
```rust
// crates/ff-graph/src/lib.rs
pub struct VaultGraph {
    pub nodes: Vec<ConceptNode>,        // from vault markdown + frontmatter
    pub edges: Vec<ConceptEdge>,        // from wiki-links + semantic analysis
    pub communities: Vec<Community>,    // Leiden clustering
    pub god_nodes: Vec<GodNode>,        // highest-degree concepts
}

impl VaultGraph {
    pub fn from_vault(vault_path: &Path) -> Self { /* native Rust, no Python */ }
    pub fn incremental_update(&mut self, changed_files: &[PathBuf]) { /* only reprocess changed */ }
    pub fn query(&self, q: &str) -> Vec<QueryResult> { /* MCP-compatible queries */ }
    pub fn to_graphify_format(&self) -> GraphifyJson { /* compatible with graph.json schema */ }
}
```

### 6.5 Graphify Integration — How It Works

**Three graph layers** (complementary, not competing):

| Layer | Tool | Source | Storage | Purpose | Updated |
|-------|------|--------|---------|---------|---------|
| **Wiki-link graph** | `ff-memory` (native) | `[[...]]` syntax | Postgres `vault_links` | Note-to-note navigation | Real-time (every sync) |
| **Concept graph** | `graphify` → `ff-graph` | Content within notes | `graphify/` (root) | Concept-to-concept relationships | Weekly or >50 file changes |
| **Code graph** | `graphify` → `ff-graph` | AST + docstrings | `graphify-codebase/` | Function-to-function, module-to-module | On git push to main |

**Future: Fleet LLM-Enhanced Extraction** (Phase 16m+):
> The fleet's local LLMs (running on each node) can augment graphify's AST extraction with semantic understanding. This is **not** a replacement — graphify's AST extraction is free (zero token cost) and fast. Fleet LLMs add:
> - **Semantic concept labels**: "This function implements the Observer pattern"
> - **Cross-project relationships**: "This auth module is similar to HireFlow360's auth"
> - **Natural language queries**: "Find all code related to user sessions"
>
> When `ff-graph` is built, it can call the local LLM via `ff-agent`'s tool system for enhanced extraction on demand.

**Vault graph location: ROOT level**

```
Yarli_KnowledgeBase/graphify/           ← vault-wide (NOT ForgeFleet/Graphify/)
Yarli_KnowledgeBase/ForgeFleet/Projects/forge-fleet/graphify-codebase/  ← codebase
```

The vault graph is at root because it maps the **entire vault** — your Daily Notes, People/, Knowledge/, Yarlagadda Home/, ForgeFleet/, everything. ForgeFleet just manages it.

**`.graphifyignore`** (minimal — process almost everything):
```
# Graphify's own output (avoid recursive self-processing)
graphify/
graphify-codebase/
*/graphify-codebase/

# Git internals
.git/

# Build artifacts
node_modules/
target/
dist/
build/
__pycache__/
*.egg-info/

# Large binary model files
*.bin
*.pkl
*.pt
*.safetensors
*.gguf
*.onnx

# Temporary / cache
*.tmp
.cache/
```

**Everything else is INCLUDED**: Daily Notes, ForgeFleet/, Projects/, People/, Knowledge/, Electronics/, Yarlagadda Home/, templates, etc.

**FF-native graph commands** (wrapping graphify with business logic):

```rust
// ff graph vault --regenerate
async fn graph_vault_regenerate(vault_path: &Path) -> GraphResult {
    // 1. Run graphify on vault root
    let output = Command::new("graphify")
        .arg("update")
        .arg(vault_path)
        .current_dir("/tmp")  // graphify-out/ goes to /tmp
        .output()?;
    
    // 2. Move output to vault root (not ForgeFleet/)
    let vault_graph_dir = vault_path.join("graphify");
    fs::remove_dir_all(&vault_graph_dir).ok();
    fs::rename("/tmp/graphify-out", &vault_graph_dir)?;
    
    // 3. Index GRAPH_REPORT.md in Postgres
    reindex_file(&vault_graph_dir.join("GRAPH_REPORT.md")).await?;
    
    // 4. Notify fleet via NATS
    nats_publish("fleet.graph.vault.updated", json!({
        "files_processed": count_files(vault_path),
        "god_nodes": extract_god_nodes(&vault_graph_dir),
    })).await?;
    
    Ok(GraphResult { nodes: ..., edges: ... })
}

// ff graph project <name> --regenerate
async fn graph_project_regenerate(project_name: &str) -> GraphResult {
    let repo_path = resolve_project_repo(project_name)?;  // ~/projects/forge-fleet/
    let vault_project_path = vault_path.join("ForgeFleet/Projects").join(project_name);
    
    Command::new("graphify")
        .arg("update")
        .arg(&repo_path)
        .current_dir("/tmp")
        .output()?;
    
    let codebase_graph_dir = vault_project_path.join("graphify-codebase");
    fs::remove_dir_all(&codebase_graph_dir).ok();
    fs::rename("/tmp/graphify-out", &codebase_graph_dir)?;
    
    reindex_file(&codebase_graph_dir.join("GRAPH_REPORT.md")).await?;
    
    Ok(GraphResult { ... })
}

// ff graph query "what connects auth to the database?"
async fn graph_query(question: &str, scope: GraphScope) -> QueryResult {
    let graph_path = match scope {
        GraphScope::Vault => vault_path.join("graphify/graph.json"),
        GraphScope::Project(p) => vault_path.join(format!("ForgeFleet/Projects/{}/graphify-codebase/graph.json", p)),
    };
    
    // Use graphify MCP server or ff-graph native query
    let mcp_client = GraphifyMcpClient::new(&graph_path).await?;
    mcp_client.query(question).await
}
```

**Taylor-only execution** (important):
- Graphify runs **only on Taylor** (leader node) for vault and codebase graphs
- Other 14 nodes **pull** updated graphs via git sync — they never run graphify themselves
- This avoids: 15× API costs, conflicting graph updates, inconsistent outputs
- Taylor has the most CPU/RAM and is always online
- If Taylor is down: graphs are stale until Taylor recovers (acceptable — graphs are "best effort", not critical path)
- Emergency: any node with graphify installed can run `ff graph vault` manually

**"Read the map before the territory":
1. FF receives task: "Add OAuth to the API"
2. FF reads `ForgeFleet/Projects/forge-fleet/graphify-codebase/GRAPH_REPORT.md`
3. Learns: god node = "AuthMiddleware", surprising connection = "auth → RateLimiter"
4. Now FF reads specific files with structural context already loaded
5. **71.5× fewer tokens** than grepping through raw files

**Auto-update strategy**:

| Graph | Trigger | Command | Frequency |
|-------|---------|---------|-----------|
| **Vault concept graph** | >50 files changed OR weekly cron | `ff graph vault --regenerate` | Weekly or on demand |
| **Codebase graph** | Git push to main | `ff graph project <name> --regenerate` | Every push |
| **Incremental** | Single file change | `graphify update .` | As needed |
| **Watch mode** | Development session | `graphify watch .` | Manual (dev only) |

**How updates work — the full flow**:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    GRAPHIFY UPDATE LIFECYCLE                                 │
│                                                                              │
│  TRIGGER (5 ways to initiate)                                               │
│  ├── 1. Periodic: Weekly cron on Taylor                                     │
│  │   └── Cron: `0 3 * * 0` (Sunday 3am) → `ff graph vault --regenerate`    │
│  ├── 2. Event-driven: ff-memory-sync detects >20 changed files             │
│  │   └── After git pull → count changed files → if >20, queue update       │
│  ├── 3. Git hook: pre-commit in project repos                              │
│  │   └── `graphify hook install` → auto-runs after each commit             │
│  ├── 4. On-demand: User or FF explicitly requests                          │
│  │   └── `ff graph vault` or `ff graph project forge-fleet`                │
│  └── 5. Watch mode: Real-time during development                           │
│      └── `graphify watch .` (Taylor only, manual start)                    │
│                              │                                               │
│                              ▼                                               │
│  EXECUTION (ff-memory daemon)                                               │
│  ├── Acquire lock: `graphify.lock` (prevent concurrent runs)               │
│  ├── Run graphify in /tmp (avoid polluting vault during processing)        │
│  │   └── `graphify update ~/Yarli_KnowledgeBase`                           │
│  ├── Output lands in /tmp/graphify-out/                                     │
│  ├── Compare with existing: hash check                                     │
│  │   └── If identical → skip, release lock, done                           │
│  │   └── If different → continue                                           │
│  ├── Move to vault: `mv /tmp/graphify-out ~/Yarli_KnowledgeBase/graphify/` │
│  ├── Re-index GRAPH_REPORT.md in Postgres                                   │
│  ├── Git commit: `ff: regenerate vault concept graph (N files changed)`    │
│  ├── Git push to origin                                                     │
│  ├── Notify fleet: NATS `fleet.graph.vault.updated`                        │
│  └── Release lock                                                           │
│                              │                                               │
│                              ▼                                               │
│  DISTRIBUTION (all 6 nodes)                                                 │
│  ├── Taylor pushes updated graphify/ folder                                 │
│  ├── Other nodes pull on next sync cycle (30s)                             │
│  ├── Each node re-indexes locally                                           │
│  └── FF on every node now reads updated graph                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

**Yes — FF calls graphify commands directly.** The `ff graph` CLI is a thin wrapper:

```rust
// crates/ff-cli/src/commands/graph.rs
pub async fn graph_vault_regenerate() -> Result<()> {
    // 1. Check if already running
    let lock = GraphifyLock::acquire("vault").await?;
    
    // 2. Run graphify in temp directory
    let tmp_dir = TempDir::new()?;
    let output = Command::new("graphify")
        .arg("update")
        .arg(&vault_path)
        .current_dir(&tmp_dir)
        .output()?;
    
    // 3. Check if output changed
    let new_hash = hash_dir(&tmp_dir.join("graphify-out"))?;
    let old_hash = read_graph_hash(&vault_graph_dir)?;
    if new_hash == old_hash {
        println!("Graph unchanged, skipping");
        return Ok(());
    }
    
    // 4. Atomically swap
    let backup = vault_graph_dir.with_extension("backup");
    fs::rename(&vault_graph_dir, &backup)?;
    fs::rename(&tmp_dir.join("graphify-out"), &vault_graph_dir)?;
    fs::remove_dir_all(&backup)?;
    
    // 5. Index in Postgres
    reindex_graph_report(&vault_graph_dir.join("GRAPH_REPORT.md")).await?;
    
    // 6. Git commit + push
    git_commit(&vault_path, &format!("ff: regenerate vault graph ({})", 
        chrono::Local::now().format("%Y-%m-%d %H:%M")))?;
    git_push(&vault_path)?;
    
    // 7. Notify fleet
    nats_publish("fleet.graph.vault.updated", json!({
        "hash": new_hash,
        "files_processed": count,
        "god_nodes": extract_god_nodes(&vault_graph_dir),
    })).await?;
    
    Ok(())
}
```

**Incremental updates vs full regeneration**:

| Scenario | Strategy | Command | Time |
|----------|----------|---------|------|
| Single file edited | Incremental | `graphify update .` | ~10s |
| 5-20 files changed | Incremental | `graphify update .` | ~30s |
| >20 files or weekly | Full | `graphify update .` | ~2-5min |
| Project codebase push | Full | `graphify update ~/projects/{project}` | ~1-3min |
| Watch mode (dev) | Real-time | `graphify watch .` | Instant |

**Lock mechanism** (prevents concurrent runs):
```rust
struct GraphifyLock {
    name: String,
}
impl GraphifyLock {
    async fn acquire(name: &str) -> Result<Self> {
        let lock_file = format!("~/.forgefleet/locks/graphify-{name}.lock");
        // Try to create lock file with PID
        // If exists and PID is alive → error("graphify already running")
        // If exists and PID is dead → steal lock
        // If doesn't exist → create and proceed
    }
}
// Lock released on Drop
```

**Local Working Cache**:
```
~/.forgefleet/memory/
├── vault_mirror/                        ← Full vault working copy
│   └── (mirrors entire Yarli_KnowledgeBase)
├── ephemeral/                           ← Temporary working files
│   └── plan.md                          ← Created for planning, deleted after
└── agents/
    └── agent-taylor-0/
        ├── compacted/                   ← Persisted session summaries
        └── logs/                        ← Persisted agent process logs
```

### 6.6 ForgeFleet/Index.md — The Entry Point

Every time ForgeFleet starts a session, it reads `ForgeFleet/Index.md` first. This file is auto-generated by FF after every sync and gives FF instant orientation.

```markdown
---
generated_by: ff-agent-taylor-0
generated_at: 2026-05-04T00:34:15Z
vault_version: 47
vault_files: 1247
vault_links: 3842
---

# ForgeFleet Vault Index

## Active Projects
| Project | Status | Last Activity | Key Notes |
|---------|--------|---------------|-----------|
| [[forge-fleet]] | active | 2026-05-04 | [[Fleet Memory Architecture]] redesign |
| [[graphify]] | active | 2026-05-03 | Added project structure to vault |
| [[OpenClaw]] | monitoring | 2026-04-28 | Waiting on upstream release |

## Today's Activity (2026-05-04)
- ✅ Completed: [[Fleet Memory Architecture]] redesign
- 🔄 In Progress: Vault sync daemon implementation
- 📋 Open: [[Git Conflict Resolution]] algorithm design

## Open Tasks (from TODOs across vault)
- [ ] Refactor [[hive_sync.rs]] for vault integration — in [[forge-fleet/Active Work]]
- [ ] Design [[git conflict resolution]] — in [[ForgeFleet/Hive Mind/Git Strategy]]
- [ ] Buy paint for living room — in [[Yarlagadda Home/Renovation]]

## Recent Changes (last 7 days)
- 2026-05-04: Created [[ForgeFleet/Projects/forge-fleet/Architecture.md]]
- 2026-05-04: Updated [[ForgeFleet/Hive Mind/Fleet Memory Architecture]]
- 2026-05-03: Linked [[taylor]] → [[YarlNas]] (hosting relationship)

## Directory Map
- **ForgeFleet/** — Fleet memory & configuration (FF-managed)
- **Projects/** — Active projects (mixed user + FF)
- **Daily Notes/** — Daily activity logs
- **Electronics/Computers/** — Hardware inventory

## Key Connections
```
taylor → YarlNas (hosts)
forge-fleet → graphify (uses for visualization)
OpenClaw → Rust Best Practices (coding standard)
```

## Computers Status
| Node | Role | Status | Load |
|------|------|--------|------|
| [[taylor]] | leader | online | 0.42 |
| [[marcus]] | standby | online | 0.18 |
| [[sophie]] | monitor | online | 0.05 |
```

**Index-first navigation flow**:
```rust
// When FF starts:
// 1. Read ForgeFleet/Index.md (~5KB, <10ms)
// 2. Load relevant project notes from Index links
// 3. Follow wiki-links as needed for deeper context
// 4. Only scan full vault when search is needed
```

### 6.7 Daily Notes Integration

ForgeFleet appends its daily activity to `Daily Notes/YYYY/YYYY-MM/YYYY-MM-dd.md`. If the day's note doesn't exist, FF creates the year folder, month folder, and the file.

```markdown
<!-- Daily Notes/2026/2026-05/2026-05-04.md -->

## ForgeFleet Activity (auto-generated)

**Session**: agent-taylor-0-20260504-003415
**Tasks**: 3 completed, 1 in progress, 2 new notes created

### Completed
- ✅ [[forge-fleet]]: Completed [[Fleet Memory Architecture]] redesign
- ✅ [[graphify]]: Added project structure to vault
- ✅ [[Yarlagadda Home]]: Researched paint colors (linked to [[Renovation Plans]])

### Decisions Made
- Decided to use [[ForgeFleet/Index.md]] as vault entry point
- Chose timestamp-based conflict resolution for user files

### New Notes Created
- [[ForgeFleet/Projects/forge-fleet/Architecture.md]]
- [[ForgeFleet/Hive Mind/Git Conflict Resolution]]

### Modified Notes
- [[ForgeFleet/Brain/Preferences.md]] — updated tool preferences
- [[ForgeFleet/Computers/taylor.md]] — updated load metrics

### Cross-References
- See [[Yarlagadda Home/Renovation Plans]] for today's home improvement research
- Related: [[Electronics/Computers/YarlNas]] (discussed storage upgrades)
```

**Daily note frontmatter** (added by FF):
```yaml
---
ff_activity: true
ff_session: agent-taylor-0-20260504-003415
ff_tasks_completed: 3
ff_notes_created: 2
---
```

### 6.8 TODO Integration

ForgeFleet scans **all** `- [ ]` and `- [x]` items across the entire vault and maintains a consolidated view.

**TODO scanning**:
```rust
// Extract TODOs from any note in the vault
async fn extract_todos(vault_path: &Path) -> Vec<TodoItem> {
    let mut todos = vec![];
    for file in walk_markdown_files(vault_path) {
        let content = fs::read_to_string(&file)?;
        for line in content.lines() {
            if line.starts_with("- [ ]") || line.starts_with("- [x]") {
                todos.push(TodoItem {
                    file: file.clone(),
                    text: line[5..].trim().to_string(),
                    done: line.starts_with("- [x]"),
                    // Extract wiki-links from the TODO text for context
                    links: extract_wiki_links(line),
                });
            }
        }
    }
    todos
}
```

**TODO behaviors**:
| Action | Behavior |
|--------|----------|
| **Read TODOs** | FF reads TODOs from any file for context |
| **Create TODOs** | FF can add `- [ ]` items to any note, or create task-specific notes |
| **Complete TODOs** | FF marks items `- [x]` when tasks are done |
| **Consolidate** | FF can create `ForgeFleet/Open Tasks.md` as a dashboard |
| **Link tasks** | TODOs use wiki-links: `- [ ] Refactor [[hive_sync.rs]]` |

**TODO merge strategy** (when syncing):
- TODO lists are **union-merged**: combine open items from both versions
- Duplicates detected by text similarity
- Completed items preserved from whichever version has them checked

### 6.9 The Sync Loop

```rust
// ff-memory-sync daemon (runs on every node, every 30s)
async fn sync_loop() {
    loop {
        // 1. Git pull from Yarli_KnowledgeBase
        let pulled = git_pull(&vault_path).await?;
        
        // 2. Regenerate Index.md if vault changed significantly
        if vault_changed_significantly(&vault_path) {
            regenerate_index_md(&vault_path).await?;
        }
        
        // 3. Detect all changed files (mtime + hash comparison)
        let changed = detect_changes(&vault_path, &memory_mirror).await?;
        
        // 4. Sync mirror
        for file in &changed {
            sync_file_to_mirror(file).await?;
        }
        
        // 5. Detect FF writes in mirror (including outside ForgeFleet/)
        let ff_changes = detect_ff_changes(&memory_mirror).await?;
        
        // 6. Apply FF changes back to vault
        for file in &ff_changes {
            apply_ff_change_to_vault(file).await?;
        }
        
        // 7. Auto-commit + push
        if !ff_changes.is_empty() {
            let msg = format!("ff: {} changes by {}", 
                ff_changes.len(), 
                get_current_agent_id()
            );
            git_commit_push(&vault_path, &msg).await?;
        }
        
        // 8. Incrementally re-index changed files in Postgres
        for file in changed.iter().chain(ff_changes.iter()) {
            reindex_file(file).await?;
        }
        
        // 9. Extract and cache TODOs
        let todos = extract_todos(&vault_path).await?;
        cache_todos_in_postgres(&todos).await?;
        
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
```

### 6.10 Conflict Resolution — Full Access Model

Since FF can edit **any file** in the vault, conflicts are more complex but also more powerful.

**Attribution tracking**:
```yaml
---
# Every FF-edited file gets this frontmatter
created_by: ff-agent-taylor-0
created_at: 2026-05-04T00:34:15Z
last_modified_by: ff-agent-taylor-0
last_modified_at: 2026-05-04T12:15:00Z
ff_session: agent-taylor-0-20260504-003415
---
```

**Conflict resolution strategy**:
```rust
enum ConflictResolution {
    AutoMerge,        // Non-overlapping sections -> merge
    UnionMergeTodos,  // TODO lists -> union of items
    LastWriteWins,    // Independent files -> newer wins
    CreateMergeFile,  // Both edited same file -> create .merge.md
    AppendActivity,   // Daily notes -> always append, never overwrite
    UserWins,         // User edited after FF -> respect user's change
}

fn resolve_conflict(vault_file: &Path, mirror_file: &Path) -> ConflictResolution {
    let vault_meta = read_frontmatter(vault_file);
    let mirror_meta = read_frontmatter(mirror_file);
    
    // Daily notes: always append
    if is_daily_note(vault_file) {
        return ConflictResolution::AppendActivity;
    }
    
    // User edited after FF's last modification
    if vault_meta.last_modified_by != "ff-" 
       && vault_mtime > mirror_meta.last_modified_at {
        return ConflictResolution::UserWins;
    }
    
    // Both versions have TODOs: union merge
    if has_todos(vault_file) && has_todos(mirror_file) {
        return ConflictResolution::UnionMergeTodos;
    }
    
    // ForgeFleet file edited by both
    if is_ff_file(vault_file) && vault_mtime > mirror_mtime {
        return ConflictResolution::CreateMergeFile;
    }
    
    // Default: last write wins
    ConflictResolution::LastWriteWins
}
```

**Merge file pattern** (for complex conflicts):
```markdown
# File: Projects/Home Renovation/Budget.md.merge.md
# Conflict detected: both you and ForgeFleet edited Budget.md
# Review and merge manually, then delete this file.

## Your version (from Obsidian)
- Paint: $150
- Flooring: $2,500

## ForgeFleet version (auto-generated)
- Paint: $150
- Flooring: $2,300 (found discount at Lowe's)
- Labor: $800
```

### 6.11 Obsidian Wiki-Links & Graph Traversal

**Every note uses Obsidian wiki-links**:
```markdown
---
title: "Coding Standards"
category: "hive_mind"
confidence: 0.95
source_agent: "agent-taylor-0"
source_node: "taylor"
created: "2026-05-04"
last_modified_by: ff-agent-taylor-0
last_modified_at: "2026-05-04T12:00:00Z"
---

# Coding Standards

## Error Handling
Always use [[thiserror]] for custom error types in Rust.
See also [[Error Handling Patterns]] and [[Rust Best Practices]].

## Related Hardware
Our [[taylor]] node runs [[YarlNas]] for model storage.

## TODOs
- [ ] Update [[Error Handling Patterns]] with new async pattern
- [x] Document [[thiserror]] usage
```

**Graph traversal for context expansion**:
```rust
// User asks: "Tell me about error handling"
// 1. Read ForgeFleet/Index.md first (fast orientation)
// 2. Search finds "Coding Standards.md"
// 3. Follow wiki-links 1-2 hops for related context
// 4. Return: "Coding Standards" + "thiserror" + "Error Handling Patterns" + "taylor"

async fn expand_context(file_path: &Path, depth: usize) -> Vec<MemoryNote> {
    let mut results = vec![load_note(file_path).await?];
    let mut visited = HashSet::new();
    visited.insert(file_path.to_string());
    
    for link in extract_wiki_links(&results[0].content) {
        let target = resolve_wiki_link(&link, &vault_path);
        if visited.insert(target.clone()) && depth > 0 {
            results.extend(expand_context(&target, depth - 1).await?);
        }
    }
    
    results
}
```

**"Least bit of information first, traverse for more"**:
- Initial query returns the matching note (small, focused)
- If user asks "tell me more", agent follows `[[...]]` links
- Each hop provides deeper detail without overwhelming context window
- Cross-boundary: `ForgeFleet/Computers/taylor.md` → `Electronics/Computers/YarlNas.md`

### 6.12 Postgres as Search Index (Not Source of Truth)

**What gets stored in Postgres**:

| Data | In Vault (Markdown) | In Postgres | Reason |
|------|---------------------|-------------|--------|
| Full note content | ✅ Yes | ❌ No | Vault is source of truth |
| Title, tags, path | ✅ Frontmatter | ✅ Yes | Fast filtering |
| Embeddings | ❌ No | ✅ Yes | Vector search needs DB |
| Wiki-link graph | ✅ `[[links]]` | ✅ Yes | Fast graph traversal |
| Full-text index | ❌ No | ✅ FTS5 | Fast text search |
| File metadata | ❌ No | ✅ Yes | Incremental sync tracking |
| TODO items | ✅ `- [ ]` | ✅ Yes | Fast task queries |
| Index.md content | ✅ Yes | ✅ Cached | Sub-second entry |

```sql
-- Vault files index
CREATE TABLE vault_files (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    vault_path TEXT NOT NULL,           -- relative path in Yarli_KnowledgeBase
    title TEXT NOT NULL,
    content_hash TEXT NOT NULL,         -- for incremental sync
    file_mtime TIMESTAMPTZ,             -- last modified time
    word_count INT,
    tags TEXT[] DEFAULT '{}',
    frontmatter JSONB DEFAULT '{}',
    embedding vector(384),              -- semantic embedding
    last_indexed_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(vault_path)
);

-- Wiki-link graph
CREATE TABLE vault_links (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_path TEXT NOT NULL,
    target_path TEXT NOT NULL,
    link_text TEXT,
    context TEXT,                       -- surrounding paragraph
    created_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(source_path, target_path, link_text)
);
CREATE INDEX idx_vault_links_src ON vault_links(source_path);
CREATE INDEX idx_vault_links_target ON vault_links(target_path);

-- Full-text search
CREATE VIRTUAL TABLE vault_fts USING fts5(
    title,
    content,
    content_rowid=rowid,
    content=vault_files
);

-- TODO items across vault
CREATE TABLE vault_todos (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_path TEXT NOT NULL REFERENCES vault_files(vault_path),
    todo_text TEXT NOT NULL,
    done BOOLEAN DEFAULT false,
    line_number INT,
    wiki_links TEXT[],                  -- extracted [[links]] from todo text
    created_at TIMESTAMPTZ DEFAULT NOW(),
    completed_at TIMESTAMPTZ
);
CREATE INDEX idx_vault_todos_done ON vault_todos(done, file_path);
CREATE INDEX idx_vault_todos_links ON vault_todos USING GIN(wiki_links);

-- Daily notes tracking
CREATE TABLE daily_notes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_path TEXT NOT NULL UNIQUE,
    note_date DATE NOT NULL,
    ff_activity BOOLEAN DEFAULT false,
    tasks_completed INT DEFAULT 0,
    notes_created INT DEFAULT 0,
    notes_modified INT DEFAULT 0,
    session_id TEXT
);
```

**Query flow**: "Tell me about my home"
```sql
-- Step 1: Read Index.md cache (instant)
SELECT content FROM vault_files WHERE vault_path = 'ForgeFleet/Index.md';

-- Step 2: Full-text search
SELECT vault_path, title, rank 
FROM vault_fts 
WHERE vault_fts MATCH 'home' 
ORDER BY rank 
LIMIT 10;

-- Step 3: Follow wiki-links (graph traversal)
SELECT target_path FROM vault_links 
WHERE source_path = 'Yarlagadda Home/Home.md';

-- Step 4: Agent reads markdown from vault
```

### 6.13 The Seven Memory Layers (in the Vault)

| Layer | Vault Path | Scope | Persistence | FF Access |
|-------|-----------|-------|-------------|-----------|
| **Hive Mind** | `ForgeFleet/Hive Mind/` | Fleet-wide | Permanent | Full (create/edit/delete) |
| **Fleet Brain** | `ForgeFleet/Brain/` | Per-user | Permanent | Full |
| **Project Memory** | `ForgeFleet/Projects/{project}/` | Per-project | Permanent | Full |
| **Agent Memory** | `ForgeFleet/Agents/{agent}/` | Per-agent | Semi-permanent | Full |
| **Daily Notes** | `Daily Notes/YYYY/YYYY-MM/` | Per-day | Permanent | Append-only |
| **User Notes** | Anywhere in vault | Per-user | Permanent | Read + write on request |
| **Sub-Agent Cache** | `~/.forgefleet/memory/` | Per-session | Ephemeral | Local only |

**Agent memory lifecycle**:
- `plan.md` — created when agent plans a task, deleted when task completes
- `compacted/` — session summaries that persist across restarts
- `logs/` — agent process logs that persist for debugging

### 6.14 Memory Context Budget

When loading memory into an LLM prompt:
```
Total budget: 2000 tokens
├── ForgeFleet/Index.md:  200 tokens (10%) — vault orientation
├── Hive Mind:            500 tokens (25%) — fleet standards
├── Fleet Brain:          400 tokens (20%) — user preferences  
├── Project Memory:       400 tokens (20%) — current project context
├── Agent Memory:         200 tokens (10%) — agent role/context
├── Daily Notes (today):  200 tokens (10%) — today's activity from Daily Notes/YYYY/YYYY-MM/
└── Vault Search:         100 tokens ( 5%) — dynamically retrieved from query
```

**Dynamic retrieval**: When user asks "tell me about my home", the 100-token "vault search" budget finds relevant notes from `Yarlagadda Home/`, and FF follows wiki-links as needed.


## 7. The 16-Phase Implementation Plan

### 7.1 Parallel Execution Timeline

```
Week:  0   1   2   3   4   5   6   7   8   9  10  11  12  13  14  15  16

Phase 0 ├┤  Graphify Fleet Install + Initial Graph Generation

Track A: Infrastructure
Phase 1  ├────────────────┤  Observability
Phase 2  ├────────────────┤  Postgres HA + DR

Track B: Real-Time Core
Phase 3       ├─────────┤  LISTEN/NOTIFY
Phase 4       ├─────────┤  NATS JetStream

Track C: Core Engine
Phase 5             ├────────┤  Security
Phase 6                ├────────────┤  Adaptive Router
Phase 7                      ├──────┤  Context Engine

Track D: Orchestration
Phase 8                         ├──────────┤  Planner-Executor
Phase 9                              ├──────────┤  A2A Protocol

Track E: Operations
Phase 10                                     ├────────┤  FinOps + Scheduling
Phase 11                                        ├───────┤  Testing
Phase 12                                           ├──────┤  Dynamic Loading

Track F: Experience
Phase 13                                              ├────────┤  TUI/Dashboard
Phase 14                                                 ├──────┤  Memory

Track G: Fleet-First Execution (THE BIG ONE)
Phase 15a                    ├────┤  Tool Registry + MCP Federation
Phase 15b                       ├──┤  Selfish Routing
Phase 15c                          ├─┤  Task Decomposition + Work Items
Phase 15d                             ├┤  Work Stealing
Phase 15e                             ├┤  Shared Workspace + Sub-Agent Model
Phase 15f                                ┤  Cleanup + Lifecycle

Track H: Fleet Memory (NEW) — Full Vault Access
Phase 16a                          ├─┤  Full Vault Access Setup
Phase 16b                             ├┤  ForgeFleet/Index.md Entry Point
Phase 16c                             ├┤  Vault Index in Postgres
Phase 16d                                ┤  Daily Notes Integration
Phase 16e                                ┤  TODO Integration
Phase 16f                                ┤  Hive Mind + Fleet Brain → Vault
Phase 16g                                ┤  Computers — Fleet Node Info
Phase 16h                                ┤  Project Memory
Phase 16i                                ┤  Agent Memory
Phase 16j                                ┤  Graph Traversal + Wiki-Links
Phase 16k                                ┤  Conflict Resolution — Full Access
Phase 16l                                ┤  Learning Pipeline → Vault
Phase 16m                                ┤  Graphify Integration
```

**Total timeline**: 16 weeks + Phase 0 (immediate). Phase 15 runs parallel with Phases 8-12. Phase 16 runs parallel with Phases 12-14.

### Deployment Gates

The plan has **three deployment gates** where FF is built, deployed fleet-wide, and validated before continuing:

```
Gate 1 (Week 2): After Phase 2 — Core daemon + Postgres HA
  ├── Build: cargo build --release --bin forgefleetd
  ├── Deploy: ./deploy/install.sh to all 6 nodes
  ├── Validate: ff health check --all-nodes
  └── Resume: Nodes read plan_state from Postgres (table: fleet_plan_state)

Gate 2 (Week 6): After Phase 7 — Security + Router + Context Engine  
  ├── Build: Release artifact with all Track C features
  ├── Deploy: Rolling restart (1 node at a time)
  ├── Validate: ff integration-test --suite core
  └── Resume: plan_state.phase = 8

Gate 3 (Week 12): After Phase 12 — Full fleet-first execution
  ├── Build: Release artifact with Tracks A-E
  ├── Deploy: Full fleet restart coordinated by Taylor
  ├── Validate: ff integration-test --suite fleet
  └── Resume: plan_state.phase = 13
```

**How nodes know to resume**: 
- Each node queries `fleet_plan_state` table on startup: `SELECT current_phase, target_phase FROM fleet_plan_state WHERE fleet_id = 'main'`
- Taylor (leader) advances `target_phase` after each gate passes validation
- Nodes auto-detect phase changes and activate new features
- If a node misses a gate (offline during deploy), it catches up on next startup via `plan_state.target_phase`

---

### Phase 0: Graphify Fleet Installation (Week 0 — Immediate)

**Goal**: Install graphify on all 6 nodes. Generate initial concept graphs for the vault and all projects. This is the foundation for everything else.

> **Why Phase 0?** Before we restructure the vault, build FF memory, or migrate data, we need graphify in place. It gives us a "map of the territory" — a concept graph of the entire vault and all codebases. Every subsequent phase uses this map.

#### 0a: Install Graphify Fleet-Wide
- [x] Install `graphify` on Taylor: `uv tool install graphifyy`
- [x] Verify installation: `graphify --help`
- [x] Distribute to reachable nodes (Marcus, Sophie, James via pipx; Ace via venv)
- [ ] Fix Priya SSH access then install graphify
- [ ] Verify on each node: `ssh node-X "graphify --help"`
- [ ] Add graphify to Fleet Software Registry (§5.5)

#### 0b: Generate Vault Concept Graph
- [x] Create `.graphifyignore` in `~/projects/Yarli_KnowledgeBase/` (minimal exclusions)
- [x] Run initial graphify on vault: `cd ~/projects/Yarli_KnowledgeBase && graphify update .`
- [x] Move output to vault root: `mv graphify-out ~/projects/Yarli_KnowledgeBase/graphify/`
- [x] Verify `GRAPH_REPORT.md` exists and is readable
- [x] Inspect "god nodes" and "surprising connections" in report
- [x] Commit to git: `git add graphify/ && git commit -m "ff: initial vault concept graph"`
> Result: 12,871 nodes, 9,749 edges, 3,186 communities from 3,192 files

#### 0c: Generate Codebase Graphs for All Projects
- [x] Discover all projects in `~/projects/` and `~/taylorProjects/` (58 repos found)
- [x] Generate graphs for key projects:
  - `forge-fleet`: 11,173 nodes, 19,325 edges
  - `mealplanr`: 335 nodes, 369 edges
  - `mealplanr-v2`: 94 nodes, 83 edges
  - `FierceFlow`: 207 nodes, 172 edges
  - `Aether`: 679 nodes, 922 edges
- [ ] Generate graphs for remaining projects (batch operation)
- [ ] Handle projects without git repos (skip or init first)
- [x] Commit codebase graphs to vault

#### 0d: Set Up Graphify Git Hooks (Pre-Commit)

> **Design decision**: Graphify runs as a **pre-commit** hook, not post-commit. This ensures the updated graph is included in the same commit, avoiding extra commits and merge conflicts.

**Install hooks in every project repo**:

```bash
#!/bin/bash
# ~/.forgefleet/scripts/install-graphify-hooks.sh

set -e
VAULT_PROJECTS_DIR="$HOME/projects/Yarli_KnowledgeBase/ForgeFleet/Projects"

# Discover projects
PROJECTS=()
for dir in ~/projects/* ~/taylorProjects/*; do
    if [ -d "$dir/.git" ]; then
        PROJECTS+=("$dir")
    fi
done

echo "Found ${#PROJECTS[@]} projects with git repos"

for project_path in "${PROJECTS[@]}"; do
    project_name=$(basename "$project_path")
    hook_path="$project_path/.git/hooks/pre-commit"
    merge_hook_path="$project_path/.git/hooks/post-merge"
    
    # Skip if hook already exists and contains graphify
    if [ -f "$hook_path" ] && grep -q "graphify-hook" "$hook_path" 2>/dev/null; then
        echo "  ✓ $project_name (already has hook)"
        continue
    fi
    
    cat > "$hook_path" << HOOK
#!/bin/bash
# Graphify pre-commit hook — includes updated graph in the commit
if ! command -v graphify &> /dev/null; then
    echo "[graphify-hook] graphify not installed, skipping"
    exit 0
fi

echo "[graphify-hook] Regenerating concept graph for $project_name..."
cd "$project_path"
graphify update . 2>&1 | tail -3

VAULT_GRAPH_DIR="$VAULT_PROJECTS_DIR/$project_name/graphify-codebase"
mkdir -p "\$VAULT_GRAPH_DIR"

if [ -d "graphify-out" ]; then
    rm -rf "\$VAULT_GRAPH_DIR"
    mv graphify-out "\$VAULT_GRAPH_DIR"
    echo "[graphify-hook] Updated vault graph"
fi

# Stage the updated graph for inclusion in this commit
cd "$VAULT_PROJECTS_DIR/../../.."
git add "ForgeFleet/Projects/$project_name/graphify-codebase/" 2>/dev/null || true
HOOK

    chmod +x "$hook_path"
    cp "$hook_path" "$merge_hook_path"
    echo "  ✓ $project_name"
done

echo "Done. Hooks installed in ${#PROJECTS[@]} projects."
```

#### 0e: Set Up Auto-Update Mechanism (Non-Hook Triggers)

> **Execution model**: 
> - **Taylor** runs the full vault concept graph (expensive, ~2-5 min, 12K+ nodes)
> - **Each node** runs project-specific graphs for repos it commits to (cheap, ~10-30s)
> - All nodes pull graph updates via git sync — no merge conflicts if graphs are regenerated before commit

- [ ] **Trigger 1 — Periodic**: Weekly cron on Taylor regenerates vault graph
  ```cron
  # Taylor crontab
  0 3 * * 0 cd ~/projects/Yarli_KnowledgeBase && graphify update .
  ```
- [ ] **Trigger 2 — Event-driven**: After `ff-memory-sync` detects >20 changed files, queue graphify update
- [x] **Trigger 3 — Git hook (pre-commit)**: Installed in 0d — regenerates graph before every commit
- [ ] **Trigger 4 — On-demand**: `ff graph vault --regenerate` and `ff graph project <name> --regenerate`
- [ ] **Trigger 5 — Watch mode** (dev only): `graphify watch <path>` for real-time updates
- [ ] Implement `graphify_update_scheduler.rs` in ff-memory daemon

> **Git LFS Decision**: Deferred. The vault graph.json is ~11MB and growing. When we build our own graph implementation (Phase 16m+), we'll evaluate Git LFS vs. generating graphs on-demand vs. storing in Postgres. For now, graphs are committed to git.

#### 0f: FF Graph Commands
- [ ] `ff graph vault` — regenerate vault graph (Taylor only)
- [ ] `ff graph project <name>` — regenerate codebase graph for project
- [ ] `ff graph query "<question>"` — query any graph via MCP
- [ ] `ff graph list` — show all graphs and their last update time
- [ ] `ff graph status` — show which graphs are stale

**Success criteria**:
- [x] `graphify` installed on Taylor (primary) and verified on all reachable nodes
- [x] `Yarli_KnowledgeBase/graphify/GRAPH_REPORT.md` exists and contains meaningful concepts
- [x] Key projects have `graphify-codebase/` folders in the vault
- [x] Pre-commit hooks installed in all project repos
- [ ] `ff graph vault` command implemented (regenerates graph in <5 minutes)
- [ ] `ff graph status` shows green across all graphs

---

### Phase 1: Observability Foundation (Weeks 1-2)

**Goal**: See what's happening in the fleet.

- [ ] Add `opentelemetry` + `tracing-opentelemetry` to `ff-agent`
- [ ] Trace every `TaskRunner::tick_once()`, `dispatch_task()`, LLM call
- [ ] Add Prometheus metrics endpoint to gateway (`/metrics`)
- [ ] Token usage histogram: `forgefleet_tokens_total{feature_id, model, task_type}`
- [ ] Deploy Grafana dashboard (on Taylor or Sophie)
- [ ] Structured JSON logging across all crates
- [ ] Vector log agent on each node → centralized Loki
- [ ] Alert policies table + webhook dispatch to Slack

**Parallel with Phase 2.**

---

### Phase 2: Postgres HA + DR (Weeks 1-2)

**Goal**: Survive Taylor going down.

- [ ] Install `pg_auto_failover` monitor on Sophie
- [ ] Configure Taylor (primary) + Marcus (standby)
- [ ] Install PgBouncer on Taylor + Marcus
- [ ] Test failover: `systemctl stop postgresql` → Marcus promoted <30s
- [ ] Daily Postgres backups to S3 (via `pg_dump` or WAL archiving)
- [ ] `agent_session` checkpoint to S3 (JSON state snapshot)
- [ ] ZFS snapshots on model storage directories

**Parallel with Phase 1.**

---

### Phase 3: Real-Time Task Queue (Weeks 2-3)

**Goal**: Workers react in milliseconds, not seconds.

- [ ] Create `notify_new_task()` trigger on `fleet_tasks` INSERT
- [ ] Add `PgListener` to `TaskRunner::spawn`
- [ ] `tokio::select!` on NOTIFY vs fallback poll
- [ ] Test: INSERT → claim within 100ms
- [ ] Add `priority` column to `fleet_tasks` (HIGH/NORMAL/LOW)
- [ ] Update claim query to order by priority, then created_at

**Parallel with Phase 4.**

---

### Phase 4: NATS JetStream + Event Bus Expansion (Weeks 2-3)

**Goal**: Reliable real-time event delivery.

- [ ] Enable NATS JetStream on Taylor's NATS server
- [ ] Create durable streams for: audit, cost, alerts, tasks
- [ ] Add new subjects:
  - `fleet.metrics.gpu.{computer}`
  - `fleet.metrics.tokens.{feature_id}`
  - `fleet.alerts.triggered`
  - `fleet.config.changed`
  - `fleet.models.loaded/unloaded`
- [ ] Dashboard + TUI both subscribe to `fleet.>`

---

### Phase 5: Security Hardening (Weeks 3-4)

**Goal**: Zero-trust agent cluster.

- [ ] mTLS between all fleet nodes (cert per node, auto-renew)
- [ ] Tool allow-list per agent step (`allowed_tools` JSONB)
- [ ] 15-minute JWT TTL for agent sessions
- [ ] Structured audit logging: every tool call → `audit_log` table
- [ ] Audit log fields: agent_id, tool, params, prompt_hash, timestamp, outcome
- [ ] Resource limits on shell tasks: cgroup memory/CPU, timeout enforcement

---

### Phase 6: Adaptive LLM Router (Weeks 4-6)

**Goal**: Intelligent routing with GPU awareness.

- [ ] Add GPU metrics to Pulse beats: utilization, VRAM, temperature
- [ ] `AdaptiveRouter` with scoring:
  ```
  score = 0.3 * load + 0.3 * queue + 0.2 * latency + 0.2 * gpu
  ```
- [ ] Circuit breaker: 5 failures → OPEN for 60s → HALF-OPEN → CLOSED
- [ ] Priority queues: chat=HIGH, background=LOW
- [ ] Model fallback chain: Claude → GPT → Gemini → local → human
- [ ] Bulkhead isolation: separate pools for chat vs batch

---

### Phase 7: Context Engine (Weeks 6-7)

**Goal**: Agents find relevant context automatically.

- [ ] Enable `pgvector` extension on Taylor's Postgres
- [ ] Embed all `vault_files` entries (384-dim vectors)
- [ ] Embed tool descriptions for dynamic retrieval
- [ ] Hybrid search: vector similarity + keyword match
- [ ] `gather_context()` returns: vault chunks + tool descriptions + session memory
- [ ] Tool retrieval: given task, return top-5 most relevant tools

---

### Phase 8: Orchestration LLM (Weeks 7-9)

**Goal**: Planner decides WHAT, executor does the work.

- [ ] `PlannerLLM` on Taylor (qwen3.5-9b): takes goal → outputs JSON plan
- [ ] Plan format:
  ```json
  {
    "steps": [{"id": 1, "name": "...", "prompt": "...", "depends_on": [], "role": "...", "model": "..."}],
    "parallel_groups": [[1, 2], [3], [4, 5]],
    "estimated_tokens": 5000,
    "strategy": "map_reduce"
  }
  ```
- [ ] `Executor` (SessionRunner): runs steps in dependency order
- [ ] 90% of calls go to cheap executor, 10% to planner
- [ ] Add `ff session plan <session_id>` CLI command

---

### Phase 9: A2A Protocol Integration (Weeks 9-11)

**Goal**: Standard inter-node communication.

- [ ] Add `ra2a` crate dependency
- [ ] A2A endpoints on each node: `POST /a2a/v1/tasks/send`
- [ ] Agent cards: each node advertises capabilities via JSON
- [ ] SSE streaming for progress updates
- [ ] Replace custom RPC (:51003) with A2A
- [ ] Archive `ff-mesh` crate

---

### Phase 10: FinOps + Project Scheduling (Weeks 11-12)

**Goal**: Control costs + schedule future work.

- [ ] Per-feature token budgets in Redis
- [ ] Budget enforcement: 429 when exceeded
- [ ] Prompt/response caching (exact match + semantic similarity)
- [ ] Batch API integration for background tasks (50% discount)
- [ ] `project_schedules` table with cron expressions
- [ ] Daily schedule tick (leader-only): evaluate cron → enqueue tasks
- [ ] Task decomposition: Planner breaks projects into scheduled subtasks

---

### Phase 11: Testing & Evaluation Pipeline (Weeks 12-13)

**Goal**: Catch quality regressions before production.

- [ ] T1 Fast Checks (<2 min, <$0.10): JSON schema, latency, cost thresholds
- [ ] T2 PR Evaluation (10-20 min, $0.50-3): 20 prompts, LLM-as-judge
- [ ] T3 Comprehensive (weekly, $10-50): Safety scan, regression suite
- [ ] Regression dataset: curated "must pass" prompts
- [ ] CI integration: GitHub Actions for T1 + T2

---

### Phase 12: Dynamic Model Loading + Quantization (Weeks 13-14)

**Goal**: Maximize GPU utilization.

- [ ] vLLM sleep mode: offload idle model weights to CPU RAM
- [ ] Queue-depth-triggered loading: 5+ pending vision tasks → load vision model
- [ ] Model presets: "coding", "chat", "vision", "analysis"
- [ ] GPU memory profiler: check fit before loading
- [ ] Blue-green model migration: zero dropped requests
- [ ] Automated quantization pipeline:
  - GPU nodes: AWQ 4-bit + Marlin kernel
  - CPU nodes: GGUF Q4_K_M
  - Benchmark: throughput + quality before accepting quant

---

### Phase 13: TUI + Dashboard v2 (Weeks 14-15)

**Goal**: Real-time visibility into the entire fleet.

**TUI additions**:
- [ ] Fleet topology panel (ASCII network map)
- [ ] Live task queue table (updates via NATS)
- [ ] GPU sparklines (60s rolling window)
- [ ] NATS message inspector (subscribe to `fleet.events.>`)
- [ ] Cost bar chart (per-feature spend vs budget)
- [ ] **NEW**: Sub-agent workspace panel (show active workspaces per node)
- [ ] **NEW**: Work items progress bar (X of Y completed)

**Dashboard additions**:
- [ ] Agent service map (D3/ReactFlow: nodes=agents, edges=A2A calls)
- [ ] Session Gantt chart (steps as bars with duration)
- [ ] GPU cluster heatmap (nodes × GPUs, color=intensity)
- [ ] Task Kanban board (drag-drop to re-prioritize)
- [ ] Real-time cost burn-down charts
- [ ] **NEW**: Work item grid (batches, status, assigned node)
- [ ] **NEW**: Cleanup dashboard (bytes freed, items deleted)

---

### Phase 14: Memory Consolidation + Procedural Learning (Weeks 15-16)

**Goal**: Agents learn from experience.

- [ ] `agent_procedures` table:
  ```sql
  CREATE TABLE agent_procedures (
      id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
      name TEXT NOT NULL UNIQUE,
      trigger_pattern TEXT NOT NULL,
      steps JSONB NOT NULL,
      success_rate FLOAT CHECK (success_rate BETWEEN 0 AND 1),
      usage_count INT DEFAULT 0,
      last_used TIMESTAMPTZ,
      created_from_session UUID REFERENCES agent_sessions(id),
      created_at TIMESTAMPTZ DEFAULT NOW()
  );
  ```
- [ ] Nightly consolidation tick:
  1. Scan completed sessions for repeated successful patterns
  2. Extract common tool sequences
  3. Upsert into `agent_procedures` if success_rate > 80%
- [ ] Memory decay: lower retrieval weight for old entries
- [ ] Fact validity windows (`valid_from`, `valid_until`)
- [ ] Episodic archive: sessions >90 days → S3, keep summary in Postgres
- [ ] Procedural memory execution: if task matches trigger_pattern, use learned steps

---

### Phase 15: Fleet-First Distributed Execution (Weeks 8-16, parallel with Phases 8-14)

**Goal**: Tools and work are fleet-allocated, not computer-allocated. **This is the default.**

#### 15a: Central Tool Registry + MCP Federation (Weeks 8-9)

- [ ] `fleet_tools` table: every node registers all its tools on startup
- [ ] Tool health heartbeat: nodes update `health_checked_at` every 30s
- [ ] Auto-prune: tools stale >5 min are marked unavailable
- [ ] MCP proxy server (`ff-mcp-proxy`): aggregates tools from all nodes
- [ ] Lazy discovery: `search_tools(query)` returns keyword-scored matches; schemas loaded on demand
- [ ] Tool routing: given tool name, find healthy node that exposes it
- [ ] All 52+ tools from `crates/ff-agent/src/tools/` registered fleet-wide
- [ ] `fleet_tool_usage` table: track every tool invocation across the fleet

#### 15b: Selfish Routing (Weeks 9-10)

- [ ] `RoutingMode` enum: `LocalFirst`, `FleetFirst`, `LocalOnly`, `Balanced`
- [ ] **Default for all new sessions: `FleetFirst`**
- [ ] Default for interactive tasks: `Balanced` (slight local preference for latency)
- [ ] Default for background tasks: `FleetFirst` (40% selfish penalty on local node)
- [ ] Capability matching: route to nodes with required GPU/tools
- [ ] Session affinity: once a session starts on a node, prefer same node for subsequent steps
- [ ] A/B test: measure fleet utilization before/after selfish routing

#### 15c: Task Decomposition + Work Items (Weeks 10-11)

- [ ] Extend `PlannerLLM` output: `parallel_groups: [[step_id, ...], ...]` + `strategy`
- [ ] `work_items` table for fine-grained batch tracking
- [ ] Batch creation: Planner defines batch size; scheduler partitions work items
- [ ] `SessionRunner::tick()` dispatches independent steps as separate `fleet_tasks` rows
- [ ] Max fan-out: 8 parallel subtasks per decomposition
- [ ] Subtask routing: each subtask gets `routing_mode = FleetFirst` by default
- [ ] Parent session tracks child task IDs; aggregates results before continuing
- [ ] Example: "read 200 documents" → 10 batches of 20 → 10 different nodes → aggregated summary

#### 15d: Work Stealing (Weeks 11-12)

- [ ] Fleet capacity table in Redis: `node:{id}:queue_depth`, `node:{id}:gpu_util`
- [ ] Steal trigger: local queue empty AND remote queue depth > 3
- [ ] Steal protocol: HTTP POST `/internal/steal` → victim returns oldest pending task
- [ ] Stolen tasks execute on thief node; results written to shared `fleet_tasks`
- [ ] Recursion guard: max steal chain length = 2 (prevent ping-pong)
- [ ] Metrics: `fleet_work_steals_total`, `fleet_steal_latency_ms`

#### 15e: Shared Workspace + Sub-Agent Model (Weeks 11-13)

- [ ] `SharedWorkspace` struct: workspace_id, owner_node, sync_method, shell_state
- [ ] Sub-agent directory structure: `~/.forgefleet/agents/agent-{n}/sub-agents/sub-agent-{m}/`
- [ ] Sub-agent workspace: `work/`, `repos/`, `artifacts/pending/`, `artifacts/promoted/`, `temp/`
- [ ] Git handling: each sub-agent gets its own clone, works on branch, promotes via patch or push
- [ ] Artifact promotion: only "used" artifacts copied to target; unused stay in pending/
- [ ] Sync methods: Git (default), NFS (optional), S3 (fallback)
- [ ] Context forwarding: working_dir + session_id + shell_state serialized with remote tool call
- [ ] Shell state persisted in Postgres (`agent_session.shell_state JSONB`)

#### 15f: Cleanup + Lifecycle (Weeks 13-14)

- [ ] Daily cleanup cron (`fleet_tasks` scheduled task)
- [ ] Temp folders: cleared every run
- [ ] Promoted artifacts: deleted after 7 days
- [ ] Pending artifacts: deleted after 30 days if never promoted
- [ ] Git folders: deleted after 60 days of no activity
- [ ] Empty directories: removed
- [ ] `subagent_cleanup_log` table tracks all deletions
- [ ] Safety checks: no uncommitted changes, no active locks, no unpushed commits
- [ ] NATS event: `fleet.cleanup.completed` with bytes_freed and items_deleted

### Phase 16: Fleet Memory Architecture — Full Vault Access (Weeks 12-16, parallel with Phases 12-14)

**Goal**: ForgeFleet has full read/write access to `~/projects/Yarli_KnowledgeBase`. `ForgeFleet/Index.md` is the entry point. Daily Notes track activity. TODOs are scanned and managed across the entire vault.

#### 16a: Full Vault Access Setup (Weeks 12-13)
- [ ] Configure `~/projects/Yarli_KnowledgeBase` as the fleet memory vault
- [ ] **ForgeFleet templates**: Create template library in `ForgeFleet/Templates/` (deferred: vault restructure)
- [ ] Migrate existing folders: `Business/` → `50 Work/Business/`, `Kids/` → `40 Life/Kids/`, etc.
- [ ] Create `ForgeFleet/` structure: `Index.md`, `Hive Mind/`, `Brain/`, `Computers/`, `Projects/`, `Agents/`
- [ ] Migrate `~/.forgefleet/hive/` → `ForgeFleet/Hive Mind/`
- [ ] Migrate `~/.forgefleet/brain/` → `ForgeFleet/Brain/`
- [ ] Migrate `~/.forgefleet/research/` → `20 Knowledge/Research/`
- [ ] Create `graphify/` at vault root (for vault-wide concept graph)
- [ ] Set up `Daily Notes/YYYY/YYYY-MM/` as activity log destination
- [ ] `ff-memory-sync` daemon: 30s loop — pull → detect → copy → commit push
- [ ] Git auto-commit with attribution: `ff: 3 changes by agent-taylor-0`

#### 16b: ForgeFleet/Index.md — Entry Point (Weeks 12-13)
- [ ] Auto-generate `ForgeFleet/Index.md` after every sync
- [ ] Index content: active projects, recent changes, open tasks, directory map, computer status
- [ ] Index cached in Postgres for sub-second reads
- [ ] FF reads Index.md first on every session start
- [ ] Index links to project overviews so FF can orient without scanning entire vault

#### 16c: Vault Index in Postgres (Weeks 12-13)
- [ ] `vault_files` table: path, title, hash, mtime, tags, frontmatter, embedding
- [ ] `vault_links` table: wiki-link graph (source, target, context)
- [ ] `vault_fts` virtual table: full-text search across ALL vault markdown
- [ ] Incremental indexing: mtime + hash tracking, only changed files reprocessed
- [ ] Initial full-index on first setup
- [ ] Index.md content cached in `vault_files` with special `is_index=true` flag

#### 16d: Daily Notes Integration (Weeks 13-14)
- [ ] FF appends activity to `Daily Notes/YYYY/YYYY-MM/YYYY-MM-dd.md`
- [ ] Auto-create daily note if it doesn't exist
- [ ] Daily note format: completed tasks, decisions, new notes, modified notes, cross-references
- [ ] YAML frontmatter: `ff_activity`, `ff_session`, `ff_tasks_completed`
- [ ] Daily notes tracked in `daily_notes` table for fast queries

#### 16e: TODO Integration (Weeks 13-14)
- [ ] Scan ALL `- [ ]` / `- [x]` items across entire vault
- [ ] `vault_todos` table: file_path, todo_text, done, wiki_links
- [ ] FF can create TODOs in any note using wiki-links: `- [ ] Refactor [[hive_sync.rs]]`
- [ ] FF can mark TODOs complete
- [ ] TODO union-merge on sync: combine open items, preserve completed
- [ ] `ForgeFleet/Open Tasks.md` auto-generated dashboard of all open tasks

#### 16f: Hive Mind + Fleet Brain → Vault (Weeks 13-14)
- [ ] Fleet standards as `.md` in `ForgeFleet/Hive Mind/`
- [ ] Personal preferences in `ForgeFleet/Brain/`
- [ ] YAML frontmatter: category, confidence, source_agent, source_node, created_by, last_modified_by
- [ ] Wiki-links connect standards: `[[Error Handling Patterns]]`
- [ ] Approval: high-confidence auto-approved; low-confidence → `.pending.md`
- [ ] User-scoped: preferences follow user across nodes via git sync

#### 16g: Computers — Fleet Node Info (Weeks 14-15)
- [ ] Each fleet node gets `ForgeFleet/Computers/{node}.md`
- [ ] YAML frontmatter: node_id, role, gpu, cpu, status, last_seen, load, models
- [ ] Wiki-links to hardware: `[[taylor]]` → links to `[[YarlNas]]` if hosting
- [ ] Auto-updated by heartbeat: status, load, last_seen
- [ ] Node info also in `fleet_nodes` table for fast queries (vault = source of truth)

#### 16h: Project Memory (Weeks 14-15)
- [ ] Project conventions in `ForgeFleet/Projects/{project}/`
- [ ] When FF works on a project, it reads `ForgeFleet/Projects/{project}/` first
- [ ] Keep `.forgefleet/` in repo for backwards compatibility
- [ ] Auto-sync: sub-agent promotions include vault changes

#### 16i: Agent Memory (Weeks 14-15)
- [ ] Agent role/learnings in `ForgeFleet/Agents/{agent}/`
- [ ] Ephemeral cache: `~/.forgefleet/memory/agents/{agent}/`
- [ ] `plan.md` — ephemeral, deleted after task
- [ ] `compacted/` — persisted session summaries
- [ ] `logs/` — persisted agent process logs

#### 16j: Graph Traversal & Wiki-Links (Weeks 15-16)
- [ ] Parse `[[WikiLinks]]` from all vault files
- [ ] Build link graph in Postgres `vault_links`
- [ ] Context expansion: follow 1-2 hops from search results
- [ ] "Least bit first" — return note, traverse for more detail
- [ ] Auto-wikilinking: link new notes to related existing notes
- [ ] Cross-boundary links: `ForgeFleet/Computers/taylor.md` → `Electronics/Computers/YarlNas.md`

#### 16k: Conflict Resolution — Full Access (Weeks 15-16)
- [ ] Attribution tracking: `last_modified_by: ff-{agent}` in frontmatter
- [ ] Daily notes: append-only, never overwrite
- [ ] TODO lists: union-merge (combine open items, preserve completed)
- [ ] User-edited files: user's version wins if modified after FF
- [ ] FF files edited by both: create `.merge.md` for review
- [ ] Git history as safety net: any change revertible

#### 16l: Learning Pipeline → Vault (Weeks 15-16)
- [ ] `learning.rs` migrated to write `.md` files with YAML frontmatter
- [ ] Auto-routing: learnings → appropriate vault folder
- [ ] High-confidence standards → `ForgeFleet/Hive Mind/`
- [ ] Personal preferences → `ForgeFleet/Brain/`
- [ ] Project conventions → `ForgeFleet/Projects/{project}/`
- [ ] Daily activity → appended to `Daily Notes/YYYY/YYYY-MM/YYYY-MM-dd.md`

#### 16m: Graphify Enhancement + ff-graph Parallel Track (Weeks 15-16)

> **Prerequisite**: Phase 0 (graphify installation) already complete. Phase 16m builds on top of it.

- [ ] **Graphify auto-update scheduler**: Implement `graphify_update_scheduler.rs` in ff-memory daemon
- [ ] **5-trigger system**: Periodic + event-driven + git hooks + on-demand + watch mode
- [ ] **Lock mechanism**: `graphify.lock` prevents concurrent runs across nodes
- [ ] **Incremental vs full**: `graphify update .` for <20 files, full regenerate for >20
- [ ] **Hash-based deduplication**: Skip commit if graph output unchanged
- [ ] **Fleet notification**: NATS `fleet.graph.vault.updated` when graph changes
- [ ] **Stale detection**: `ff graph status` shows which graphs are out of date
- [ ] **MCP server integration**: `python -m graphify.serve graph.json` for live queries
- [ ] **Git hooks fleet-wide**: `graphify hook install` on all project repos
- [ ] **Parallel track — ff-graph crate**:
  - [ ] `vault_graph.rs` — native vault markdown + frontmatter parser
  - [ ] `wiki_graph.rs` — `[[wiki-link]]` graph builder (no regex, proper Obsidian resolution)
  - [ ] `concept_extractor.rs` — semantic extraction (replaces graphify LLM for vault content)
  - [ ] `community_detector.rs` — Leiden clustering in Rust
  - [ ] `incremental_updater.rs` — real-time updates on file change (no batch)
  - [ ] `fleet_sync.rs` — sync graph updates across 6 nodes via NATS
- [ ] **Switch criteria**: ff-graph replaces graphify when it achieves ≥90% feature parity

---

## 8. Database Schema

### 8.1 Core Tables (Existing)

```sql
-- Existing tables (already in production)
-- fleet_nodes, fleet_tasks, agent_sessions, agent_steps, projects, etc.
```

### 8.2 Phase 3: Task Priority

```sql
ALTER TABLE fleet_tasks ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal'
    CHECK (priority IN ('critical', 'high', 'normal', 'low', 'batch'));
CREATE INDEX idx_fleet_tasks_priority_created ON fleet_tasks(priority, created_at)
    WHERE status = 'pending';
```

### 8.3 Phase 5: Audit Logging

```sql
CREATE TABLE audit_log (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    params JSONB NOT NULL DEFAULT '{}',
    prompt_hash TEXT,
    outcome TEXT NOT NULL CHECK (outcome IN ('success', 'failure', 'denied')),
    created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX idx_audit_log_agent ON audit_log(agent_id, created_at);
```

### 8.4 Phase 10: Project Scheduling

```sql
CREATE TABLE project_schedules (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    cron_expression TEXT NOT NULL,
    next_run_at TIMESTAMPTZ NOT NULL,
    task_template JSONB NOT NULL,
    enabled BOOLEAN DEFAULT true,
    last_run_at TIMESTAMPTZ,
    run_count INT DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

### 8.5 Phase 14: Procedural Memory

```sql
CREATE TABLE agent_procedures (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL UNIQUE,
    trigger_pattern TEXT NOT NULL,
    steps JSONB NOT NULL,
    success_rate FLOAT CHECK (success_rate BETWEEN 0 AND 1),
    usage_count INT DEFAULT 0,
    last_used TIMESTAMPTZ,
    created_from_session UUID REFERENCES agent_sessions(id),
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

### 8.6 Phase 15a: Fleet Tool Registry

```sql
CREATE TABLE fleet_tools (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_name TEXT NOT NULL,
    node_id UUID REFERENCES fleet_nodes(id) ON DELETE CASCADE,
    description TEXT NOT NULL,
    parameters_schema JSONB NOT NULL,
    capabilities_required TEXT[] DEFAULT '{}',
    health_checked_at TIMESTAMPTZ DEFAULT NOW(),
    call_count INT DEFAULT 0,
    avg_latency_ms FLOAT,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(tool_name, node_id)
);
CREATE INDEX idx_fleet_tools_name ON fleet_tools(tool_name);
CREATE INDEX idx_fleet_tools_node_health ON fleet_tools(node_id, health_checked_at);
```

### 8.7 Phase 15a: Tool Usage Tracking

```sql
CREATE TABLE fleet_tool_usage (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_name TEXT NOT NULL,
    node_id UUID REFERENCES fleet_nodes(id),
    session_id UUID REFERENCES agent_sessions(id),
    task_id UUID REFERENCES fleet_tasks(id),
    work_item_id UUID REFERENCES work_items(id),
    subagent_id TEXT,
    input_summary TEXT,
    started_at TIMESTAMPTZ DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    latency_ms INT,
    success BOOLEAN,
    tokens_in INT DEFAULT 0,
    tokens_out INT DEFAULT 0,
    cost_usd FLOAT DEFAULT 0.0,
    workspace_path TEXT
);
CREATE INDEX idx_tool_usage_tool ON fleet_tool_usage(tool_name, started_at);
CREATE INDEX idx_tool_usage_node ON fleet_tool_usage(node_id, started_at);
CREATE INDEX idx_tool_usage_session ON fleet_tool_usage(session_id);
```

### 8.8 Phase 15c: Work Items (Adaptive Weighted)

```sql
CREATE TABLE work_items (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    parent_task_id UUID NOT NULL REFERENCES fleet_tasks(id) ON DELETE CASCADE,
    batch_id INT NOT NULL,
    item_index INT NOT NULL,
    item_key TEXT NOT NULL,
    item_type TEXT NOT NULL,
    item_metadata JSONB DEFAULT '{}',

    -- Weighted estimation (NEW)
    estimated_weight FLOAT NOT NULL DEFAULT 1.0,
    actual_weight FLOAT,
    complexity_factors JSONB DEFAULT '{}',

    assigned_node_id UUID REFERENCES fleet_nodes(id),
    assigned_agent_id TEXT,
    assigned_session_id UUID REFERENCES agent_sessions(id),
    claimed_at TIMESTAMPTZ,

    -- Progress tracking (NEW)
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'claimed', 'in_progress', 'completed', 'failed', 'yielded', 'stolen')),
    progress_percent INT DEFAULT 0,
    checkpoint_data JSONB DEFAULT '{}',
    yielded_at TIMESTAMPTZ,
    stolen_from UUID REFERENCES fleet_nodes(id),

    result_summary TEXT,
    result_artifact_id UUID,
    result_tokens_in INT DEFAULT 0,
    result_tokens_out INT DEFAULT 0,
    completed_at TIMESTAMPTZ,
    error_message TEXT,

    retry_count INT DEFAULT 0,
    max_retries INT DEFAULT 2,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(parent_task_id, item_index)
);
CREATE INDEX idx_work_items_parent ON work_items(parent_task_id, status);
CREATE INDEX idx_work_items_batch ON work_items(parent_task_id, batch_id, status);
CREATE INDEX idx_work_items_claimed ON work_items(assigned_node_id, status)
    WHERE status IN ('claimed', 'in_progress');
CREATE INDEX idx_work_items_yielded ON work_items(parent_task_id, status)
    WHERE status = 'yielded';
```

### 8.9 Phase 15c: Task Decomposition Fields

```sql
ALTER TABLE fleet_tasks ADD COLUMN IF NOT EXISTS parent_session_id UUID REFERENCES agent_sessions(id);
ALTER TABLE fleet_tasks ADD COLUMN IF NOT EXISTS parent_step_id UUID REFERENCES agent_steps(id);
ALTER TABLE fleet_tasks ADD COLUMN IF NOT EXISTS agent_depth INT DEFAULT 0;
ALTER TABLE fleet_tasks ADD COLUMN IF NOT EXISTS delegation_chain UUID[] DEFAULT '{}';
ALTER TABLE fleet_tasks ADD COLUMN IF NOT EXISTS routing_mode TEXT DEFAULT 'fleet_first'
    CHECK (routing_mode IN ('local_first', 'fleet_first', 'local_only', 'balanced'));
ALTER TABLE fleet_tasks ADD COLUMN IF NOT EXISTS batch_id INT;
ALTER TABLE fleet_tasks ADD COLUMN IF NOT EXISTS strategy TEXT DEFAULT 'sequential'
    CHECK (strategy IN ('sequential', 'map_reduce', 'pipeline', 'vote', 'competitive', 'fanout_gather'));
```

### 8.10 Phase 15c: Work Batches

```sql
CREATE TABLE work_batches (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    parent_task_id UUID NOT NULL REFERENCES fleet_tasks(id) ON DELETE CASCADE,
    batch_index INT NOT NULL,
    total_estimated_weight FLOAT NOT NULL DEFAULT 0,
    total_actual_weight FLOAT,
    items_count INT NOT NULL,
    assigned_node_id UUID REFERENCES fleet_nodes(id),
    assigned_agent_id TEXT,
    assigned_session_id UUID REFERENCES agent_sessions(id),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'claimed', 'in_progress', 'completed', 'rebalancing')),
    progress_percent INT DEFAULT 0,
    rebalanced_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(parent_task_id, batch_index)
);
```

### 8.11 Phase 15e: Fleet Workspaces

```sql
CREATE TABLE fleet_workspaces (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_node_id UUID REFERENCES fleet_nodes(id),
    workspace_path TEXT NOT NULL,
    sync_method TEXT NOT NULL CHECK (sync_method IN ('git', 'nfs', 's3')),
    sync_config JSONB NOT NULL DEFAULT '{}',
    shell_state JSONB DEFAULT '{}',
    last_synced_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

### 8.12 Phase 15f: Cleanup Tracking

```sql
CREATE TABLE subagent_cleanup_log (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    node_id UUID REFERENCES fleet_nodes(id),
    subagent_id TEXT NOT NULL,
    item_type TEXT NOT NULL CHECK (item_type IN ('git_folder','artifact','temp','empty_dir')),
    item_path TEXT NOT NULL,
    bytes_freed BIGINT,
    reason TEXT NOT NULL,
    deleted_at TIMESTAMPTZ DEFAULT NOW()
);
```

### 8.13 Phase 17: Config Versioning

```sql
CREATE TABLE fleet_config_versions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    key TEXT NOT NULL,
    value JSONB NOT NULL,
    changed_by TEXT,
    changed_at TIMESTAMPTZ DEFAULT NOW(),
    previous_value JSONB
);
```

### 8.14 Phase 7: pgvector

```sql
CREATE EXTENSION IF NOT EXISTS vector;
ALTER TABLE rag_chunks ADD COLUMN embedding vector(384);
CREATE INDEX idx_rag_chunks_embedding ON rag_chunks USING ivfflat (embedding vector_cosine_ops);
```

### 8.15 Phase 16: Vault Files Index

```sql
CREATE TABLE vault_files (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    vault_path TEXT NOT NULL,
    title TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    file_mtime TIMESTAMPTZ,
    word_count INT,
    tags TEXT[] DEFAULT '{}',
    frontmatter JSONB DEFAULT '{}',
    embedding vector(384),
    last_indexed_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(vault_path)
);
CREATE INDEX idx_vault_files_path ON vault_files(vault_path);
CREATE INDEX idx_vault_files_tags ON vault_files USING GIN(tags);
```

### 8.16 Phase 16: Vault Wiki-Link Graph

```sql
CREATE TABLE vault_links (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_path TEXT NOT NULL,
    target_path TEXT NOT NULL,
    link_text TEXT,
    context TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(source_path, target_path, link_text)
);
CREATE INDEX idx_vault_links_src ON vault_links(source_path);
CREATE INDEX idx_vault_links_target ON vault_links(target_path);
```

### 8.17 Phase 16: Vault Full-Text Search

```sql
-- FTS5 virtual table for full-text search across vault markdown
CREATE VIRTUAL TABLE vault_fts USING fts5(
    title,
    content,
    content_rowid=rowid,
    content=vault_files
);
```

### 8.18 Phase 16: Memory Sync Log

```sql
CREATE TABLE memory_sync_log (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    layer TEXT NOT NULL CHECK (layer IN ('hive', 'brain', 'project', 'agent', 'daily_note', 'todo', 'user_note')),
    file_path TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('created', 'updated', 'deleted', 'merged', 'appended', 'moved')),
    source_node_id UUID REFERENCES fleet_nodes(id),
    source_agent_id TEXT,
    conflict_resolved BOOLEAN DEFAULT false,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX idx_memory_sync_layer ON memory_sync_log(layer, created_at);
```

### 8.19 Phase 16: Vault TODOs

```sql
CREATE TABLE vault_todos (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_path TEXT NOT NULL REFERENCES vault_files(vault_path),
    todo_text TEXT NOT NULL,
    done BOOLEAN DEFAULT false,
    line_number INT,
    wiki_links TEXT[],                  -- extracted [[links]] from todo text
    created_at TIMESTAMPTZ DEFAULT NOW(),
    completed_at TIMESTAMPTZ
);
CREATE INDEX idx_vault_todos_done ON vault_todos(done, file_path);
CREATE INDEX idx_vault_todos_links ON vault_todos USING GIN(wiki_links);
```

### 8.20 Phase 16: Daily Notes

```sql
CREATE TABLE daily_notes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_path TEXT NOT NULL UNIQUE,
    note_date DATE NOT NULL,
    ff_activity BOOLEAN DEFAULT false,
    tasks_completed INT DEFAULT 0,
    notes_created INT DEFAULT 0,
    notes_modified INT DEFAULT 0,
    session_id TEXT
);
CREATE INDEX idx_daily_notes_date ON daily_notes(note_date);
CREATE INDEX idx_daily_notes_ff ON daily_notes(ff_activity, note_date);
```

---

## 9. Component Architecture

### 9.1 Rust Modules

```
crates/
  ff-agent/src/
    task_runner.rs              ← Phase 3: PgListener + batch claiming
    inference_router.rs         ← Phase 6+15b: AdaptiveRouter + SelfishRouting
    orchestrator_agent.rs       ← Phase 8: PlannerLLM + Executor
    session_runner.rs           ← Phase 8+15c: DAG execution + fan-out
    memory_consolidation.rs     ← Phase 14: NEW
    scheduler_tick.rs           ← Phase 10: cron evaluation
    nats_log_layer.rs           ← Phase 1: Structured logging
    audit_logger.rs             ← Phase 5: NEW
    resilient_router.rs         ← Phase 6: NEW - circuit breaker layer
    tool_registry.rs            ← Phase 15a: NEW - fleet_tools management
    mcp_proxy.rs                ← Phase 15a: NEW - aggregates remote tools
    work_stealer.rs             ← Phase 15d: NEW - proactive load balancing
    shared_workspace.rs         ← Phase 15e: NEW - remote context forwarding
    task_decomposer.rs          ← Phase 15c: NEW - PlannerLLM DAG output
    batch_manager.rs            ← Phase 15c: NEW - work_items partition/claim
    work_estimator.rs           ← Phase 15c: NEW - item complexity estimation
    progress_tracker.rs         ← Phase 15c: NEW - batch progress monitoring
    yield_manager.rs            ← Phase 15c: NEW - yield/steal/resume protocol
    artifact_promoter.rs        ← Phase 15e: NEW - promotes used artifacts
    cleanup_service.rs          ← Phase 15f: NEW - daily cleanup cron

  ff-terminal/src/views/
    fleet_topology.rs           ← Phase 13: NEW
    task_queue.rs               ← Phase 13: NEW
    gpu_metrics.rs              ← Phase 13: NEW
    nats_stream.rs              ← Phase 13: NEW
    cost_dashboard.rs           ← Phase 13: NEW
    subagent_workspace.rs       ← Phase 15e: NEW - workspace panel
    work_items_grid.rs          ← Phase 15c: NEW - batch progress
    cleanup_dashboard.rs        ← Phase 15f: NEW - cleanup stats

  ff-gateway/src/
    metrics_endpoint.rs         ← Phase 1: NEW - /metrics
    alert_dispatcher.rs         ← Phase 1: NEW
    a2a_handler.rs              ← Phase 9: NEW
    tool_registry_api.rs        ← Phase 15a: NEW - /api/tools fleet-wide catalog
    steal_endpoint.rs           ← Phase 15d: NEW - POST /internal/steal
    batch_api.rs                ← Phase 15c: NEW - /api/batches claim/release
    workspace_api.rs            ← Phase 15e: NEW - /api/workspace sync

  ff-brain/src/
    procedural_memory.rs        ← Phase 14: NEW
    consolidation.rs            ← Phase 14: NEW

  ff-memory/src/ (NEW MODULE for Phase 16)
    vault_store.rs              ← Phase 16a: Full vault mirror in ~/.forgefleet/memory/
    vault_indexer.rs            ← Phase 16c: Incremental markdown index → Postgres
    vault_watcher.rs            ← Phase 16c: fsnotify + mtime-based change detection
    vault_sync.rs               ← Phase 16a: Git pull/push + conflict resolution
    vault_restructure.rs        ← Phase 16a: Migrates existing folders to numbered structure
    index_generator.rs          ← Phase 16b: Auto-generates ForgeFleet/Index.md
    daily_notes.rs              ← Phase 16d: Appends activity to Daily Notes/YYYY/YYYY-MM/
    todo_extractor.rs           ← Phase 16e: Scans/manages TODOs across vault
    todo_merge.rs               ← Phase 16e: Union-merge TODO lists on sync
    wiki_link_parser.rs         ← Phase 16j: [[WikiLink]] extraction from .md
    link_graph.rs               ← Phase 16j: Traversal queries over vault_links
    computer_notes.rs           ← Phase 16g: Fleet node markdown auto-generation
    conflict_resolver.rs        ← Phase 16k: Full-access attribution + merge
    memory_context.rs           ← Phase 16i: SubAgentMemoryContext serialization
    learning_router.rs          ← Phase 16l: Routes learnings → vault folder
    memory_budget.rs            ← Phase 16f: Token budget enforcement per layer
    graphify_runner.rs          ← Phase 16m: Executes graphify CLI on vault/codebase
    graphify_report_reader.rs   ← Phase 16m: Parses GRAPH_REPORT.md for context
    graphify_mcp_client.rs      ← Phase 16m: Queries graph.json via MCP server
    graphify_ignore.rs          ← Phase 16m: Manages .graphifyignore

  ff-graph/src/ (NEW CRATE — Phase 16m parallel track)
    vault_graph.rs              ← Extracts concepts from vault markdown + frontmatter
    wiki_graph.rs               ← Native [[wiki-link]] graph builder
    concept_extractor.rs        ← Semantic concept extraction (replaces graphify LLM)
    community_detector.rs       ← Leiden clustering on vault graph
    god_node_finder.rs          ← Identifies highest-degree concepts
    report_generator.rs         ← Generates GRAPH_REPORT.md-compatible output
    graph_query.rs              ← MCP-compatible query engine
    incremental_updater.rs      ← Real-time graph updates on file change
    fleet_sync.rs               ← Syncs graph updates across 6 nodes via NATS
```

### 9.2 Dashboard Components

```
dashboard/src/components/
  AgentServiceMap.tsx         ← Phase 13: NEW
  SessionGanttChart.tsx       ← Phase 13: NEW
  GpuHeatmap.tsx              ← Phase 13: NEW
  TaskKanbanBoard.tsx         ← Phase 13: NEW
  CostBurnDown.tsx            ← Phase 13: NEW
  RealtimeAlertBanner.tsx     ← Phase 1: NEW
  WorkItemsGrid.tsx           ← Phase 15c: NEW - batch status table
  SubagentWorkspacePanel.tsx  ← Phase 15e: NEW - per-node workspace list
  CleanupDashboard.tsx        ← Phase 15f: NEW - cleanup history + stats
  VaultIndexPanel.tsx         ← Phase 16b: NEW - ForgeFleet/Index.md viewer
  HiveMindBrowser.tsx         ← Phase 16a: NEW - fleet standards browser
  BrainPreferences.tsx        ← Phase 16b: NEW - user preference editor
  DailyNotesFeed.tsx          ← Phase 16d: NEW - FF activity timeline
  TodoDashboard.tsx           ← Phase 16e: NEW - all vault TODOs consolidated
  ComputerStatusPanel.tsx     ← Phase 16g: NEW - fleet node markdown cards
  AgentMemoryPanel.tsx        ← Phase 16i: NEW - per-agent memory viewer
  MemorySyncStatus.tsx        ← Phase 16a: NEW - sync health across layers
  GraphifyReportViewer.tsx    ← Phase 16m: NEW - concept graph browser
```

### 9.3 High-Level System Diagram

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              CLIENT LAYER                                    │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────┐  │
│  │ ff CLI   │  │ TUI      │  │ Dashboard│  │ Telegram │  │ MCP Client   │  │
│  │ (shell)  │  │(ratatui) │  │ (React)  │  │ (bot)    │  │ (:50001)     │  │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘  └──────┬───────┘  │
│       │             │             │             │               │          │
│       └─────────────┴─────────────┴─────────────┘               │          │
│                         │                                       │          │
│                         ▼                                       ▼          │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                            GATEWAY (:51002)                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │ Task Router  │  │ Chat API     │  │ A2A Handler  │  │ Metrics Endpoint │  │
│  │ (8 types)    │  │ (/v1/chat)   │  │ (/a2a/v1)    │  │ (/metrics)       │  │
│  ├──────────────┤  ├──────────────┤  ├──────────────┤  ├──────────────────┤  │
│  │ Batch API    │  │ Tool Registry│  │ Steal API    │  │ Workspace API    │  │
│  │ (/api/batch) │  │ (/api/tools) │  │ (/internal)  │  │ (/api/workspace) │  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  └────────┬─────────┘  │
│         │                 │                  │                   │          │
│         └─────────────────┴──────────────────┘                   │          │
│                              │                                    │          │
└──────────────────────────────┼────────────────────────────────────┼──────────┘
                               │                                    │
                               ▼                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         ORCHESTRATION LAYER                                  │
│                                                                              │
│  ┌──────────────────┐  ┌──────────────────┐  ┌────────────────────────────┐  │
│  │ Planner LLM      │  │ Session Runner   │  │ Adaptive Router            │  │
│  │ (qwen3.5-9b)     │  │ (DAG executor)   │  │ (GPU-aware + selfish)      │  │
│  │                  │  │                  │  │                            │  │
│  │ Goal → JSON Plan │  │ Steps → Tasks    │  │ FleetFirst by default      │  │
│  │ + parallel_groups│  │ + batch claims   │  │ Local = last resort        │  │
│  └──────────────────┘  └──────────────────┘  └────────────────────────────┘  │
│                                                                              │
│  ┌──────────────────┐  ┌──────────────────┐  ┌────────────────────────────┐  │
│  │ Task Runner      │  │ Batch Manager    │  │ Work Stealer               │  │
│  │ (SKIP LOCKED)    │  │ (work_items)     │  │ (idle → steals)            │  │
│  │ LISTEN/NOTIFY    │  │ partition/claim  │  │ proactive balancing        │  │
│  └──────────────────┘  └──────────────────┘  └────────────────────────────┘  │
│                                                                              │
│  ┌──────────────────┐  ┌──────────────────┐  ┌────────────────────────────┐  │
│  │ Tool Registry    │  │ Artifact Promoter│  │ Cleanup Service            │  │
│  │ (fleet_tools)    │  │ (pending → target)│  │ (30d/60d cleanup)          │  │
│  │ lazy discovery   │  │ only used items  │  │ sub-agent lifecycle        │  │
│  └──────────────────┘  └──────────────────┘  └────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                            DATA & EVENT LAYER                                │
│  ┌──────────────────┐  ┌──────────────────┐  ┌────────────────────────────┐  │
│  │ PostgreSQL       │  │ NATS JetStream   │  │ Redis                      │  │
│  │ (primary + HA)   │  │ (event bus)      │  │ (caching + pub/sub)        │  │
│  │                  │  │                  │  │                            │  │
│  │ fleet_tasks      │  │ fleet.events.>   │  │ token_budgets              │  │
│  │ agent_sessions   │  │ fleet.metrics.>  │  │ circuit_breaker_state      │  │
│  │ work_items       │  │ fleet.alerts.>   │  │ node_capacity              │  │
│  │ fleet_tools      │  │ fleet.cleanup.>  │  │ model_cache                │  │
│  │ vault_files      │  │ fleet.memory.*   │  │ vault_fts_cache            │  │
│  │ vault_links      │  │ fleet.config.>   │  │                            │  │
│  └──────────────────┘  └──────────────────┘  └────────────────────────────┘  │
│                                                                              │
│  ┌──────────────────┐  ┌──────────────────┐  ┌────────────────────────────┐  │
│  │ Loki (logs)      │  │ Prometheus       │  │ S3 (backups + archive)     │  │
│  │ Vector agents    │  │ (metrics)        │  │ session checkpoints        │  │
│  │                  │  │ Grafana dashboards│  │ model weights              │  │
│  └──────────────────┘  └──────────────────┘  └────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                           FLEET NODES (x15)                                  │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────┐  │
│  │ Taylor   │  │ Marcus   │  │ Sophie   │  │ Ace      │  │ Blaze        │  │
│  │ (Leader) │  │ (Standby)│  │ (Monitor)│  │ (Worker) │  │ (Worker)     │  │
│  │          │  │          │  │          │  │          │  │              │  │
│  │ Planner  │  │ Postgres │  │pg_auto_  │  │ vLLM     │  │ vLLM         │  │
│  │ Scheduler│  │ Standby  │  │failover  │  │ Models   │  │ Models       │  │
│  │ NATS     │  │ PgBouncer│  │ Monitor  │  │ TaskRunner│  │ TaskRunner   │  │
│  └──────────┘  └──────────┘  └──────────┘  └──────────┘  └──────────────┘  │
│                                                                              │
│  Each node runs:                                                             │
│  - ff-agent (with all 52 tools)                                              │
│  - agents in ~/.forgefleet/agents/agent-{0..3}/                               │
│  - sub-agents in ~/.forgefleet/agents/agent-{n}/sub-agents/sub-agent-{0..3}/  │
│  - MCP server (:50001) exposing local tools                                  │
│  - Gateway (:51002) for inter-node API                                       │
│  - Vault sync daemon (~/projects/Yarli_KnowledgeBase)                         │
│                                                                              │
│  mTLS mesh + A2A Protocol + Pulse heartbeats (every 10s)                    │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 9.4 Hierarchical Agent Workspace Lifecycle

```
FLEET (6 nodes)
    │
    ▼
COMPUTER (Node X)
    │
    ├──► AGENT-0 (primary)
    │      ├── config/           ← role, capabilities, tool allow-list
    │      ├── memory/           ← episodic, semantic, procedural
    │      ├── workspace/        ← agent's own working directory
    │      └── SUB-AGENTS/
    │          ├── sub-agent-0/  ← isolated workspace
    │          │   ├── work/     ← tool execution
    │          │   ├── repos/    ← per-project git clones
    │          │   ├── artifacts/← pending/ + promoted/
    │          │   └── temp/     ← cleared every run
    │          ├── sub-agent-1/
    │          ├── sub-agent-2/
    │          └── sub-agent-3/
    │
    └──► AGENT-1 (secondary — different project/role)
           └── SUB-AGENTS/
               ├── sub-agent-0/
               ├── sub-agent-1/
               ├── sub-agent-2/
               └── sub-agent-3/

Lifecycle per sub-agent:
┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   CREATE    │────▶│   EXECUTE   │────▶│  PROMOTE    │────▶│   CLEANUP   │
└─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘
       │                   │                   │                   │
       ▼                   ▼                   ▼                   ▼
  mkdir workspace    tools run here      used artifacts     daily cron
  git clone repo     artifacts in        copied to target   deletes:
  set working_dir    pending/            git: patch/push    - temp/ (always)
                     temp/ cleared       unused: left       - pending/ (>30d)
                     on teardown         behind             - repos/ (>60d)
                                                             - empty dirs
```

---

## 10. Validation Checklist

### Infrastructure (Phases 1-2)
- [ ] **HA**: `systemctl stop postgresql` on Taylor → Marcus promoted within 30s
- [ ] **HA**: Zero data loss on failover
- [ ] **HA**: PgBouncer survives without app reconnection
- [ ] **Observability**: Every task claim has a trace span
- [ ] **Observability**: Grafana shows per-feature cost vs budget
- [ ] **Observability**: Alert fires when budget exceeds 80%
- [ ] **Observability**: Structured JSON logs in Loki
- [ ] **DR**: Daily Postgres backup to S3
- [ ] **DR**: Session checkpoint restore works

### Real-Time (Phases 3-4)
- [ ] **Queue**: INSERT → worker claims within 100ms
- [ ] **Queue**: Fallback poll still works if NOTIFY fails
- [ ] **NATS**: Critical events persisted through restart
- [ ] **NATS**: Dashboard receives events after reconnect

### Core Engine (Phases 5-7)
- [ ] **Security**: Inter-node traffic uses mTLS
- [ ] **Security**: Tool call without allow-list → denied + logged
- [ ] **Security**: Audit log contains full lineage
- [ ] **Router**: Overloaded node (GPU 95%) gets no new requests
- [ ] **Router**: Circuit breaker trips after 5 failures
- [ ] **Router**: Chat task routed before background task
- [ ] **Router**: Fallback chain works (Claude → GPT → local)
- [ ] **Context**: Vault query returns relevant chunks
- [ ] **Context**: Tool retrieval returns top-5 relevant tools
- [ ] **Context**: Hybrid search beats pure vector

### Orchestration (Phases 8-9)
- [ ] **Planner**: Complex prompt → JSON plan with dependencies
- [ ] **Planner**: 90% of LLM calls go to cheap executor
- [ ] **A2A**: Node A → Node B via A2A with SSE progress
- [ ] **A2A**: Third-party A2A agent can discover fleet capabilities

### Operations (Phases 10-12)
- [ ] **FinOps**: Feature exceeding budget returns 429
- [ ] **FinOps**: Caching reduces duplicate calls 20%+
- [ ] **Scheduling**: Cron task enqueues at correct time
- [ ] **Scheduling**: Daily digest summarizes upcoming tasks
- [ ] **Testing**: T1 fast checks run in <2 min on every commit
- [ ] **Testing**: T2 PR evaluation runs on prompt changes
- [ ] **Testing**: T3 weekly scan finds zero new safety issues
- [ ] **Dynamic**: Idle model sleeps within 30s
- [ ] **Dynamic**: Model wakes within 5s of queue depth > threshold
- [ ] **Dynamic**: Blue-green migration: zero dropped requests
- [ ] **Quantization**: AWQ models run 1.6× faster than FP16
- [ ] **Quantization**: Quality degradation <5% on benchmarks

### Fleet-First Execution (Phase 15) — THE DEFAULT
- [ ] **Tool Registry**: All 6 nodes register their tools within 30s of startup
- [ ] **Tool Registry**: `search_tools("bash")` returns all Bash tools across fleet
- [ ] **Tool Registry**: Stale tools (>5 min) auto-marked unavailable
- [ ] **Tool Usage**: `fleet_tool_usage` shows which node ran which tool when
- [ ] **Selfish Routing**: Background task on Node 1 routes to Node 3+ when fleet has capacity
- [ ] **Selfish Routing**: `FleetFirst` is the DEFAULT routing mode
- [ ] **Selfish Routing**: Interactive task can still route locally for <50ms latency
- [ ] **Selfish Routing**: Session affinity keeps multi-step workflows on same node
- [ ] **Task Decomposition**: `ff "research X"` spawns ≥3 parallel subtasks on different nodes
- [ ] **Task Decomposition**: "Read 200 documents" → **weighted partition** (variable batch sizes) → 10 nodes
- [ ] **Task Decomposition**: Fast batch (light docs) finishes in 30s; slow batch (heavy docs) takes 5min
- [ ] **Task Decomposition**: Fast workers **steal individual items** from slow batches
- [ ] **Task Decomposition**: Yield protocol: slow worker checkpoints and releases remaining items
- [ ] **Task Decomposition**: Resume protocol: fast worker resumes from checkpoint, no duplicate work
- [ ] **Task Decomposition**: Subtask results aggregated before returning to user
- [ ] **Task Decomposition**: Max recursion depth = 3; deeper delegation blocked
- [ ] **Work Stealing**: Idle node with empty queue steals from node with queue depth > 3
- [ ] **Work Stealing**: Stolen task executes successfully; result visible to original requestor
- [ ] **Work Stealing**: Fine-grained: steal individual items, not whole batches
- [ ] **Work Stealing**: No ping-pong: steal chain length ≤ 2
- [ ] **Shared Workspace**: Remote node can `Read` file from requestor's workspace
- [ ] **Shared Workspace**: Shell state (cwd, env) preserved across remote executions
- [ ] **Hierarchical Workspace**: `agents/agent-{n}/sub-agents/sub-agent-{m}/` structure
- [ ] **Hierarchical Workspace**: Multiple agents per computer for project/role isolation
- [ ] **Sub-Agent Workspace**: Each sub-agent works in isolated workspace under its parent agent
- [ ] **Sub-Agent Workspace**: Git repos cloned per-sub-agent, not shared
- [ ] **Artifact Promotion**: Only used artifacts copied to target; unused stay in pending/
- [ ] **Artifact Promotion**: Images/videos: only referenced ones promoted
- [ ] **MCP Federation**: `Bash` tool executed on Node 7 via MCP proxy from Node 1
- [ ] **Cost Tracking**: Delegation tree budget enforced; subtask returns 429 if exceeded
- [ ] **Cleanup**: Temp folders cleared every run
- [ ] **Cleanup**: Pending artifacts deleted after 30 days
- [ ] **Cleanup**: Git folders deleted after 60 days of inactivity
- [ ] **Cleanup**: Agent workspaces cleaned up when agent is destroyed
- [ ] **Cleanup**: Cleanup event published to NATS with bytes_freed

### Phase 0 — Graphify Foundation (BEFORE everything else)
- [ ] **Install**: `graphify` installed on all 6 nodes (Taylor via uv, others via pipx/venv)
- [ ] **Verify**: `graphify --help` works on every node
- [ ] **Vault graph**: `Yarli_KnowledgeBase/graphify/GRAPH_REPORT.md` exists and contains concepts
- [ ] **Vault graph**: God nodes and surprising connections are meaningful
- [ ] **Codebase graphs**: Every project in `~/projects/` has `graphify-codebase/`
- [ ] **Codebase graphs**: Every project in `~/taylorProjects/` has `graphify-codebase/`
- [ ] **.graphifyignore**: Created in vault root with minimal exclusions
- [ ] **Auto-update**: Weekly cron scheduled on Taylor for vault graph
- [ ] **Auto-update**: Git hooks installed in all project repos
- [ ] **FF commands**: `ff graph vault`, `ff graph project`, `ff graph query` work
- [ ] **FF status**: `ff graph status` shows all graphs green
- [ ] **Performance**: Vault graph regenerates in <5 minutes
- [ ] **Lock**: Concurrent runs prevented by `graphify.lock`

### Fleet Memory (Phase 16) — FULL VAULT ACCESS
- [ ] **Full Access**: ForgeFleet can read/write/delete/move/merge any file in the vault
- [ ] **Attribution**: Every FF edit has `last_modified_by: ff-{agent}` in frontmatter
- [ ] **Vault Sync**: `~/projects/Yarli_KnowledgeBase` is source of truth for all memory
- [ ] **Vault Sync**: Auto-sync daemon runs every 30s on every node
- [ ] **Vault Sync**: Git commit messages include agent attribution: `ff: 3 changes by agent-taylor-0`
- [ ] **Index.md**: `ForgeFleet/Index.md` auto-generated after every sync
- [ ] **Index.md**: FF reads Index.md first on every session start (<10ms)
- [ ] **Index.md**: Index contains: active projects, recent changes, open tasks, computer status
- [ ] **Daily Notes**: FF appends activity to `Daily Notes/YYYY/YYYY-MM/YYYY-MM-dd.md`
- [ ] **Daily Notes**: Auto-creates daily note if missing
- [ ] **Daily Notes**: Cross-references link FF activity to user notes
- [ ] **TODOs**: ALL `- [ ]` items across vault scanned and indexed
- [ ] **TODOs**: FF can create TODOs in any note with wiki-links
- [ ] **TODOs**: FF can mark TODOs complete
- [ ] **TODOs**: TODO union-merge on sync (combine open, preserve completed)
- [ ] **Hive Mind**: Markdown files in `ForgeFleet/Hive Mind/` with YAML frontmatter
- [ ] **Hive Mind**: High-confidence entries auto-approved; low-confidence → `.pending.md`
- [ ] **Fleet Brain**: Personal preferences in `ForgeFleet/Brain/`
- [ ] **Computers**: Each fleet node has `ForgeFleet/Computers/{node}.md`
- [ ] **Computers**: Node markdown auto-updated by heartbeat (status, load, last_seen)
- [ ] **Computers**: Wiki-links connect nodes to hardware (e.g., `[[taylor]]` → `[[YarlNas]]`)
- [ ] **Vault Restructure**: Numbered prefixes (00-99) applied to all folders
- [ ] **Vault Restructure**: Existing folders migrated to new locations
- [ ] **Vault Restructure**: `~/.forgefleet/hive/` → `ForgeFleet/Hive Mind/`
- [ ] **Vault Restructure**: `~/.forgefleet/brain/` → `ForgeFleet/Brain/`
- [ ] **Vault Restructure**: `~/.forgefleet/research/` → `20 Knowledge/Research/`
- [ ] **Project Memory**: When FF works on a project, it reads project overview first
- [ ] **Graphify**: Vault graph at ROOT: `graphify/` (not under ForgeFleet/)
- [ ] **Graphify**: Each project has `graphify-codebase/` with code structure graph
- [ ] **Graphify**: FF reads `GRAPH_REPORT.md` before working on any project
- [ ] **Graphify**: Vault graph auto-regenerates weekly or on >50 file changes
- [ ] **Graphify**: Codebase graphs auto-regenerate on git push to main
- [ ] **Graphify**: `.graphifyignore` excludes `graphify/` and `graphify-codebase/` from re-processing
- [ ] **Graphify**: `.graphifyignore` is minimal — almost everything is processed
- [ ] **Graphify**: `--wiki` output creates agent-crawlable markdown articles
- [ ] **Graphify**: FF-native commands: `ff graph vault`, `ff graph project`, `ff graph query`
- [ ] **ff-graph (Rust)**: Begins parallel development for long-term graphify replacement
- [ ] **Agent Memory**: Agent role loaded on startup from `ForgeFleet/Agents/{agent}/`
- [ ] **Conflict Resolution**: Daily notes append-only, never overwrite
- [ ] **Conflict Resolution**: User-edited files: user's version wins if modified after FF
- [ ] **Conflict Resolution**: TODO lists union-merge
- [ ] **Conflict Resolution**: Complex conflicts create `.merge.md` for review
- [ ] **Vault Index**: Postgres `vault_files` + `vault_links` + `vault_todos` + `daily_notes` updated within 5s
- [ ] **Vault Index**: FTS5 search returns results in <100ms across all vault markdown
- [ ] **Wiki-Links**: `[[WikiLinks]]` parsed from all vault files, graph in Postgres
- [ ] **Wiki-Links**: Cross-boundary links work (ForgeFleet/ → Electronics/ → Yarlagadda Home/)
- [ ] **Memory Budget**: Index 10% + Hive 25% + Brain 20% + Project 20% + Daily 10% + Search 5% = 100%
- [ ] **Learning Pipeline**: Session learnings auto-routed to correct vault folder
- [ ] **Learning Pipeline**: High-confidence standards written to `ForgeFleet/Hive Mind/` automatically
- [ ] **Learning Pipeline**: Daily activity appended to `Daily Notes/YYYY/YYYY-MM/` automatically

### Experience (Phases 13-14)
- [ ] **TUI**: Real-time fleet topology visible
- [ ] **TUI**: Task queue updates without polling
- [ ] **TUI**: GPU sparklines show 60s history
- [ ] **TUI**: Sub-agent workspace panel shows active workspaces per node
- [ ] **Dashboard**: Agent service map renders all interactions
- [ ] **Dashboard**: Session Gantt chart shows step durations
- [ ] **Dashboard**: GPU heatmap shows all nodes
- [ ] **Dashboard**: Task Kanban board supports drag-drop priority
- [ ] **Dashboard**: Work items grid shows batch progress
- [ ] **Memory**: Nightly consolidation creates ≥1 procedure/week
- [ ] **Memory**: Old episodic memories archived to S3
- [ ] **Memory**: Fact validity windows respected in retrieval

---

## 11. Risk Assessment & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Postgres HA adds complexity | Medium | High | Start with pg_auto_failover (simplest option). Test failover repeatedly. |
| A2A migration breaks existing RPC | Medium | High | Run A2A in parallel with custom RPC during transition. Gradual cutover. |
| pgvector performance on large data | Low | Medium | Start with existing vault data (~thousands of docs). Monitor query times. Scale to Qdrant if needed. |
| Planner LLM quality inconsistent | Medium | Medium | Fallback to keyword matching if planner output is invalid JSON. Human review of plans. |
| Dynamic model loading causes OOM | Medium | High | GPU memory profiler prevents loading models that won't fit. Sleep mode frees VRAM before loading. |
| Cost overruns on cloud fallback | Medium | High | Hard budget stops (429). Alert at 50%/80%. Daily cost review. |
| Circuit breaker too aggressive | Low | Medium | Tune thresholds with production data. Start conservative (10 failures). |
| Memory consolidation creates bad procedures | Low | Medium | Require 80% success rate. Human review queue for new procedures. |
| 16-week timeline slips | Medium | Medium | Parallel tracks reduce critical path. Can ship incremental value after Phase 7. |
| Fleet-first breaks local tool assumptions | Medium | High | Start with read-only tools remotely. Gradually enable write tools with workspace sync. |
| Selfish routing causes all-remote thrashing | Low | Medium | Session affinity + soft penalty (not hard). Fallback to local if all remote >80% load. |
| Work stealing creates duplicate execution | Low | High | Stolen tasks marked `status = 'stolen'` atomically. Victim never executes stolen tasks. |
| Shared workspace sync lag | Medium | Medium | Git sync is default (fast). NFS only for large files. Conflict resolution = last-write-wins. |
| Sub-agent workspace disk bloat | Medium | High | Daily cleanup cron with 30d/60d policy. Cleanup events logged. Users can tune retention. |
| Artifact promotion loses work | Low | High | Sub-agent marks artifacts as "used" explicitly. Parent reviews promotion list. Undo window: 24h. |
| Adaptive partitioning estimates wrong | Medium | Medium | Actual weight recorded after completion. Estimator learns from history. Fallback to equal-split if scan fails. |
| Yield protocol leaves dangling checkpoints | Low | Medium | Checkpoints auto-expire after 24h. Cleanup cron removes stale checkpoints. |
| Hierarchical agent structure too complex | Low | Medium | Start with 1 agent per node. Multi-agent is opt-in via config. Flat mode available. |
| Fast worker steals too aggressively | Low | Medium | Steal cooldown: 30s between steals. Max 3 items stolen per cycle. Prevent thrashing. |
| FF accidentally modifies user notes | Medium | High | Attribution frontmatter shows what FF changed. Git revert any commit. Daily Notes/ and ForgeFleet/ are append-only safe zones. Start with read-only on user folders, enable writes gradually. |
| Git merge conflicts in vault | Medium | High | Timestamp-based: newer version wins. TODOs union-merge. Complex conflicts → `.merge.md`. User edits always win over FF if user modified last. |
| Vault sync lag (60s git cycle) | Medium | Medium | NATS `fleet.memory.*` for urgent updates. Git sync for persistence. Emergency sync API for immediate push. Index.md cached locally for instant reads. |
| Daily notes become cluttered | Low | Medium | FF appends structured sections. User can delete or archive old daily notes. Cleanup cron archives notes >90 days. |
| Index.md becomes stale | Low | Medium | Regenerated every sync (30s). If vault changes significantly (>10 files), immediate regeneration. Cached in Postgres as fallback. |
| Vault file corruption | Low | High | Git history is the backup. Every change committed. Can restore any file to any point in time. |
| Postgres index out of sync | Medium | Medium | mtime + hash-based incremental indexing. Full re-index on startup if checksum mismatch. Health check alerts on drift. |
| Agent memory becomes stale | Medium | Medium | TTL in frontmatter: 90 days default. Relevance decay over time. Nightly cleanup of obsolete learnings. |
| FF creates too many notes | Low | Medium | Consolidation: related learnings merged into single note. Nightly deduplication. User can delete any ForgeFleet/ note. |
| TODO list divergence | Low | Medium | Union-merge on sync. Deduplication by text similarity. If same TODO in multiple files, FF prefers the most specific context. |
| Vault restructure breaks existing links | Low | High | **Deferred.** If restructure happens later, FF auto-updates `[[...]]` links. Atomic git commit with `git revert` rollback. |
| Node connectivity issues | High | High | Priya SSH key auth failing. Fix: verify authorized_keys, check SSH service, ensure correct IP (.106). Document all nodes in fleet.toml. |
| Graphify output becomes stale | Low | Medium | Weekly auto-regen for vault graph. Pre-commit hook auto-regen for codebase graphs. `graphify update .` for incremental refresh. |
| Graphify processes its own output | Low | High | `.graphifyignore` excludes all `graphify/` and `graphify-codebase/` folders. Verified on every run. |
| Graphify API costs for docs/images | Medium | Medium | Code processed locally (free). Docs/images only processed when changed. `--no-cluster` for AST-only (zero API cost). |
| Git repo size balloons with graph JSON | Low | Medium | `graph.json` can be 1-5MB. Use Git LFS or `.gitattributes` for `graph.json`. Or commit only `GRAPH_REPORT.md` + `wiki/` and generate `graph.json` on demand. |
| Migration from ~/.forgefleet/ loses data | Low | High | Migrate incrementally. Keep ~/.forgefleet/ as backup until vault is verified. Git history preserves everything. Audit trail in `memory_sync_log`. |

---

## 12. Appendix: Research Sources

### Task Queue & Distributed Systems
- pgqueuer (GitHub) — LISTEN/NOTIFY pattern
- PostgreSQL documentation — SKIP LOCKED, LISTEN/NOTIFY

### Multi-Agent Orchestration
- LangGraph — Graph-based state machines, time-travel debugging
- CrewAI — Role-based teams, YAML config
- AutoGen — Conversational patterns, human-in-the-loop
- OpenAgents — Network-based, MCP + A2A native
- AgentGit — Git-like rollback/branching for agents

### Postgres HA
- pg_auto_failover (Citus Data) — 2-node HA without etcd
- Patroni documentation — comparison
- VMware recommendations — simple HA scenarios

### LLM Routing
- SMG (lightseekorg/smg) — KV cache-aware routing, WASM plugins
- TensorZero — <1ms p99 latency, unified API
- llmlb — TPS-based load balancing, GPU-aware
- octoroute — Rule-based + LLM-powered routing
- OneUptime blog — GPUMetrics, ReplicaStatus, adaptive scoring

### Inter-Node Protocols
- Google A2A Protocol (March 2026) — Agent2Agent standard
- ra2a (qntx/ra2a) — Comprehensive Rust SDK
- a2a-rs (EmilLindfors) — Hexagonal architecture
- MCP (Anthropic) — Model Context Protocol

### Observability
- OpenTelemetry Rust v0.27 — stable logs/metrics, beta traces
- Maxim AI — Agent-specific observability platform
- New Relic (Feb 2026) — Agent Service Map, drill-down
- Langfuse — Production tracing → evaluation datasets
- AgentGateway — Prometheus metrics for token usage

### Security
- OpenClaw AI Framework — WASM sandboxing default
- GenMind — Zero-trust + sandboxing defense in depth
- Blaxel — MicroVM architecture for agent isolation
- Zylos — DID + Verifiable Credentials for agent identity
- OWASP Agentic Top 10 (2026)
- ACRFence — Semantic rollback attack prevention

### Memory Systems
- Tulving (1972) — Episodic/Semantic/Procedural taxonomy
- arXiv:2512.13564 — Memory in the Age of AI Agents
- Letta — Filesystem benchmark (74%)
- Mem0 — Managed memory platform
- Zep — Temporal knowledge graphs
- AWS AgentCore — Long-term memory
- FileGRAMOS — Procedural memory from filesystem

### Obsidian Vault as Knowledge Graph
- obra/knowledge-graph (GitHub) — Obsidian vault → SQLite + vector embeddings + graph algorithms (Leiden, PageRank)
- ClaudeVault (reddit) — Git sync for Obsidian with intelligent conflict resolution
- Obsidian.md — Native wiki-link `[[...]]` syntax, graph view, community plugins
- Dataview plugin — SQL-like queries over vault frontmatter
- Graph Analysis plugin — Centrality, pathfinding, community detection in vault graph
- Markdown as source of truth — Human-readable, git-diffable, outlives any app

### Codebase Knowledge Graphs
- **graphify** (safishamsi/graphify, 42k+ stars, MIT) — Folder → knowledge graph. Tree-sitter AST + LLM semantic extraction. Outputs `graph.html`, `GRAPH_REPORT.md`, `graph.json`. `--wiki` for agent-crawlable articles. `--obsidian` for vault output. MCP server mode.
- **GitNexus** (14k stars, PolyForm NC) — Deepest MCP integration. TypeScript. Restrictive license.
- **CodeGraphContext** (2.2k stars, MIT) — Python graph DB + MCP. More complex setup.
- **Repomix** (22k stars, MIT) — Context packing, not true graph. XML-structured repo flattening.
- **Sourcetrail** (GPL) — Interactive code exploration. C/C++/Java/Python.
- **Code2Vec** — Function-level embeddings. No graph structure.

### Context / RAG
- RAGFlow — Deep document understanding
- Qdrant — Rust vector database
- pgvector — Postgres vector extension
- "RAG is dead, long live Context Engineering" (2025)

### Auto-Scaling
- vLLM sleep mode — CPU RAM offloading
- SwapServeLLM (SC '25) — Engine-agnostic hot-swap
- Sardeenz (Red Hat) — Control plane for multi-model
- KEDA — Kubernetes autoscaling on queue depth

### FinOps
- FinOps Foundation — LLM cost tracking best practices
- Cloudidr — 250× pricing spread analysis
- Zop.dev — Per-feature budget enforcement
- Gravitee — Agent mesh cost optimization

### Testing
- DeepEval (Confident AI) — Pytest-style agent testing
- promptfoo — Red-teaming, vulnerability scanning
- Giskard — Automated LLM safety scanning
- Ragas — Faithfulness scoring for RAG

### TUI
- ratatui.rs — 3600+ crates ecosystem
- k9s — Kubernetes TUI
- btm/bottom — System monitor
- zenith — Cross-platform monitoring
- yozefu — Kafka TUI
- toktop — LLM usage monitor

### Dashboard
- React + Vite + WebSocket — Existing stack
- D3.js / ReactFlow — Graph visualization
- New Relic Agent Monitoring — Service map pattern

### Project Management
- Motion — AI daily schedule optimization
- ClickUp Brain — Task generation, auto-prioritization
- Monday.com — Risk prediction, resource visibility
- Taskade — Multi-agent collaboration
- Epicflow — Multi-project resource management

### Resilience
- Thinkata — Graceful degradation hierarchies
- k8s4claw — 3 retries + degrade to human
- Circuit Breaker Pattern — Distributed System Authority
- Resilience Patterns skill — Circuit breaker + bulkhead + retry

### Model Quantization
- Prem.ai — GGUF vs AWQ vs GPTQ comparison (2026)
- ai.rs — Quantization methods with benchmarks
- JarvisLabs — AWQ + Marlin kernel benchmarks
- cast.ai — SmoothQuant, bitsandbytes

### Fleet Operations
- FIDO Device Onboard — Zero-touch provisioning
- FleetDM — SLSA attestation, automated software install
- A-Bots — Secure IoT lifecycle management

### Log Aggregation
- Grafana Loki — Log aggregation without resource hunger
- Vector (Timber.io) — Rust-based log router
- Fluent Bit — Lightweight collection agent
- Parseable — Unified observability platform

### Alerting
- PagerDuty — Multi-channel notification
- OneUptime — Webhook, SMS, email configuration
- Spike.sh — Alert fatigue handling

### Backup / DR
- Crab (arXiv 2604.28138) — Semantics-aware checkpoint/restore
- MoEvement — Sparse checkpointing for MoE models
- GlusterFS — Snapshot and replication strategies
- AWS DR strategies — Automation and readiness

### Fleet-First Distributed Execution (Phase 5 Research)
- A2A and MCP in 2026 (dev.to/chunxiaoxx) — Dual stack: MCP for toolplane, A2A for coordination
- A2A vs MCP for AI Coding (AugmentCode, 2026) — Protocol boundaries, production adoption gaps
- AI Agent A2A Reference (techbytes.app, 2026) — Agent Cards, task lifecycle, REST binding
- Enterprise MCP Part Two (Insight FactSet, 2025) — MCP proxy pattern, dynamic registration, remote execution
- Building AI Coding Agents for Terminal (arXiv 2603.05344) — OpenDev lazy tool discovery, token reduction
- MCP Integration Fabric (ajithp.com, 2025) — Central tool registry, containerized microservice pattern
- Kubernetes Anti-Affinity (k8s.io, 2026) — Pod anti-affinity rules translated to selfish routing
- Scalable Load Balancing (HAL 02405735) — Work stealing schedulers, topology-aware stealing
- Hybrid Parallel Task Placement (JPDC 2019) — Locality-flexible task migration, 32% speedup
- OpenClaw Multi-Agent Orchestration (sparkco.ai, 2026) — Map-Reduce, Fan-Out/Fan-In, consensus patterns
- AI Agent Orchestration Patterns (Azure, 2026) — Concurrent orchestration, scatter-gather
- Multi-Agent AI Systems for Enterprise (swfte.com, 2025) — Sequential pipeline, parallel fan-out/fan-in
- Agentic Design Patterns (SitePoint, 2026) — Orchestrator-Worker dynamic decomposition
- Task-Adaptive Multi-Agent Orchestration (arXiv 2602.16873) — AdaptOrch 5-phase pipeline
- Ray Clusters for AI (Introl, 2026) — Distributed task placement, actor model, heterogeneous scheduling
- Celery Distributed Task Queues (OneUptime, 2025) — Task routing, result backends, worker scaling

---

*Compiled from 5 research phases, 75+ sources, covering distributed systems, multi-agent orchestration, database HA, LLM routing, observability, security, memory systems, TUI/dashboard design, project management, resilience, model optimization, fleet operations, log aggregation, alerting, disaster recovery, and **fleet-first distributed execution with sub-agent workspaces and work queue partitioning**.*

**Document version**: 2026.5.5_4  
**Next step**: Begin Phase 1 + Phase 2 + Phase 15a scaffolding (parallel tracks). Phase 16 (Fleet Memory) can start scaffolding once Phase 2 (Postgres HA) is stable.
