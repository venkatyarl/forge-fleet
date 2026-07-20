#![allow(
    dead_code,
    private_interfaces,
    clippy::collapsible_if,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    clippy::nonminimal_bool,
    clippy::too_many_arguments
)]

//! ForgeFleet Virtual Brain — knowledge graph, context selection, chat, and fact extraction.
//!
//! This crate provides the core intelligence layer for ForgeFleet's Virtual Brain system:
//! - **vault**: Obsidian vault parser and indexer (markdown, frontmatter, wikilinks, chunking)
//! - **context**: Smart context selector with graph-aware retrieval
//! - **chat**: Channel-agnostic chat service (Discord, CLI, web, etc.)
//! - **facts**: Knowledge node extraction from assistant responses
//! - **embeddings**: Local MLX embedding client (stub until server deployed)
//! - **communities**: Leiden community detection on the vault graph (connected-components fallback)

pub mod chat;
pub mod communities;
pub mod community_summary;
pub mod context;
pub mod corpus;
pub mod cortex;
pub mod cortex_embed;
pub mod cortex_reindex;
pub mod data_index;
pub mod distillation;
pub mod doc_index;
pub mod embeddings;
pub mod facts;
pub mod image_index;
pub mod procedural_memory;
pub mod stack_backlog;
pub mod train_conflict;
pub mod vault;
pub mod vector_search;

pub use chat::{
    ThreadSummary, attach_thread, create_thread, get_attached_thread, list_threads,
    receive_message, resolve_user,
};
pub use communities::{CommunitySummary, detect_code_communities, detect_communities};
pub use community_summary::{
    CommunitySummaryStats, SummarizeOpts, build_summary_prompt, clean_summary,
    pick_served_model_id, resolve_served_model_id, spawn_summary_refresh_loop,
    summarize_communities,
};
pub use context::{BrainMessage, ContextBundle, ResolvedNode, select_context};
pub use cortex::ingest_decisions;
pub use cortex::ingest_pm;
pub use cortex::work_item_context::{
    CodeSnippet, RelatedWorkItem, RelevantFile, WorkItemContext, WorkItemContextExtractor,
    precompute_context, related_work_items, relevant_code_snippets, relevant_files, write_context,
};
pub use cortex::{
    CommunityExplanation, CommunityMember, CortexAlerts, CortexStats, FileOutline, GraphExport,
    OutlineEntry, SymbolHit, SymbolRef, SymbolSource, TestHit, call_path, callees,
    callees_all_corpora, callers, callers_all_corpora, cortex_alerts, cortex_search,
    explain_community, export_neighborhood, find_symbols, find_symbols_all_corpora,
    find_symbols_semantic, graph_to_dot, graph_to_graphml, impact, impact_all_corpora, index,
    outline_file, show_symbol, tests_for,
};
pub use cortex_embed::{EmbedStats, embed_cortex_nodes, spawn_embed_refresh_loop};
pub use cortex_reindex::{reindex_self_corpus, spawn_reindex_loop};
pub use embeddings::{
    EmbeddingClient, fleet_embedding_client, fleet_rerank, generate_embedding,
    generate_embedding_with_pool,
};
pub use facts::{contains_signal_phrases, extract_candidates};
pub use procedural_memory::{consolidate, spawn_consolidation_loop};
pub use stack_backlog::{BacklogItem, BrainStateClient, StackItem};
pub use vault::{
    IndexReport, ParsedNode, VaultChunk, VaultConfig, chunk_markdown, extract_wikilinks,
    index_changed_files, index_vault, parse_frontmatter, parse_vault_file, spawn_vault_index_tick,
};
pub use vector_search::{VaultNode, hybrid_search, vector_search};
