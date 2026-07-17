# Cortex FalkorDB Backend Migration Plan

This is a design and scaffold plan only. Postgres remains the production Cortex
backend until dual-write, backfill, and read-parity tests prove FalkorDB.

## Current Postgres Surface

Cortex stores its structural graph in the shared Brain knowledge graph tables:

- `brain_vault_nodes`: all `content:file`, `code:*`, `db:*`, `http:*`, `config:*`,
  `event:*`, `security:*`, `person:*`, `project:*`, `decision:*`, and other
  extracted nodes. Cortex code symbols use stable paths like
  `code://<corpus>/<qualified_name>`.
- `brain_vault_edges`: typed relationships. Core code graph edges are
  `contains`, `imports`, and `calls`; universal graph extractors add `reads`,
  `writes`, `publishes`, `subscribes`, `guards`, `implements`, `tracked_by`, and
  similar domain edges.
- `brain_vault_nodes.embedding vector(1024)`: bge-m3 embeddings for semantic
  search.
- `cortex_file_index`: per-corpus, per-file content hash ledger for incremental
  reindex.
- `cortex_reexports`: per-file Rust facade/re-export ledger used to resolve
  incremental call targets.
- `cortex_generations`: per-corpus current-generation pointer and indexing lock
  metadata. Readers filter to generation `0` or `current_generation`.
- `brain_code_communities`: code-community registry used by `cortex explain`.

Main write paths:

- Full index wipes `code:%` nodes for a corpus, clears file/reexport ledgers,
  extracts files, runs universal graph extractors, then publishes a generation.
- Incremental index diffs `content:file.content_hash` against `cortex_file_index`,
  deletes removed files' symbols, clears outgoing edges for changed files,
  upserts stable path-keyed nodes, writes `contains`/`imports`/`calls`, records
  file hashes and reexports, garbage-collects orphan import/extern placeholders,
  then publishes a generation.
- `cortex_embed.rs` fills NULL embeddings on `code:`, `doc:`, `data:`, and
  `image:` nodes.
- Community detection labels code nodes with `code_community_id` and writes
  `brain_code_communities`.

Main read paths:

- `find_symbols`: substring match over `code:*`, ranked by call fan-in.
- `find_symbols_semantic`: query embedding plus pgvector distance over
  `brain_vault_nodes.embedding`.
- `cortex_search`: semantic search, one-hop callers/callees expansion, rerank.
- `callers` / `callees`: one-hop `calls` traversals with confidence filtering.
- `impact`: reverse transitive caller closure.
- `call_path`: shortest forward `calls` path.
- `tests_for`: reverse `calls` closure filtered by test-file/name heuristics.
- `show_symbol`: resolve symbol, find owning file via transitive `contains`, read
  source from disk.
- `outline_file`: descend `contains` from a `content:file`.
- `explain_community`: resolve symbol, read `code_community_id`, registry
  summary, parent subsystem chain, and high-fan-in members.
- MCP exposes these plus domain extractors: dependencies, DB readers/writers,
  config keys, endpoints/API, external services, event topics, security gates,
  owners, product features, logs/errors, and review risk maps.

## Trait Boundary

The new `cortex::storage::CortexGraphStore` trait is the intended graph-data
boundary. It covers:

- idempotent node upsert;
- idempotent edge upsert;
- corpus code-node wipe;
- embedding persistence;
- symbol discovery, semantic discovery, callers, callees, impact, call path,
  tests, community explanation, and file outline.

The first implementation, `PostgresCortexGraphStore`, delegates to the existing
Postgres helpers and preserves current behavior. `FalkorCortexGraphStore` is a
compile-time scaffold that owns a Redis/FalkorDB connection and records the
Cypher/openCypher query shapes.

Ledgers and generation locking intentionally remain Postgres-owned for the first
step. Moving those behind the trait should happen only after graph read/write
parity is measured.

## FalkorDB Mapping

Use one FalkorDB graph per fleet, initially named `cortex`.

Node label:

- `(:CortexNode {id, path, title, node_type, project, start_line, end_line,
  generation, confidence, provenance, valid_until, code_community_id, embedding})`

Relationship types map directly from `brain_vault_edges.edge_type`:

- `(:CortexNode)-[:contains]->(:CortexNode)`
- `(:CortexNode)-[:imports]->(:CortexNode)`
- `(:CortexNode)-[:calls {confidence, provenance, method, evidence,
  generation}]->(:CortexNode)`
- Universal graph edges keep their existing edge type names.

Indexes:

- range index on `CortexNode.path`;
- range index on `CortexNode.project`;
- full-text index on `CortexNode.title`;
- vector index on `CortexNode.embedding` with dimension `1024` and cosine
  similarity.

FalkorDB supports OpenCypher and vector indexes. The intended semantic query is:

```cypher
CALL db.idx.vector.queryNodes('CortexNode', 'embedding', $limit, vecf32($embedding))
YIELD node, score
WHERE node.project = $project AND node.node_type STARTS WITH 'code:'
RETURN node, score
ORDER BY score DESC
```

## Dual-Write And Backfill

1. Add config:
   - `cortex_graph_backend=postgres|falkordb`
   - `cortex_graph_dual_write=true|false`
   - `FALKORDB_URL=redis://127.0.0.1:63379`
   - `FALKORDB_GRAPH=cortex`
2. Keep reads on Postgres while `dual_write=true`.
3. During index and embed passes, write Postgres first. If FalkorDB write fails,
   log and mark a metric; do not fail production indexing during the shadow
   period.
4. Add `ff cortex backfill-falkor --corpus <slug>`:
   - read live generation-filtered nodes and edges from Postgres;
   - upsert nodes to FalkorDB by stable `path`;
   - upsert edges by `(src_id, dst_id, edge_type)`;
   - copy embeddings;
   - copy `code_community_id`.
5. Add parity checks:
   - node counts by `project,node_type`;
   - edge counts by `project,edge_type`;
   - top fan-in symbols;
   - callers/callees/impact/call_path fixtures;
   - semantic top-k overlap for representative queries.
6. Switch one non-critical corpus to `cortex_graph_backend=falkordb` for reads.
7. Promote only when MCP/CLI parity and latency are acceptable.

## Testing

- Unit-test the backend-neutral selection logic and any FalkorDB response
  decoder with recorded `redis::Value` fixtures.
- Integration-test FalkorDB in compose on host port `63379`.
- Golden-query tests for `find`, `callers`, `callees`, `impact`, `path`,
  `tests`, `outline`, and semantic search against a tiny fixture corpus.
- Backfill idempotency test: run backfill twice and assert stable counts.
- Failure-mode test: FalkorDB unavailable with `dual_write=true` must not break
  Postgres indexing.
- Performance smoke test: compare p95 for callers/callees/impact and semantic
  top-k before read cutover.
