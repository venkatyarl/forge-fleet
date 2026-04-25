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
pub mod context;
pub mod embeddings;
pub mod facts;
pub mod stack_backlog;
pub mod vault;

pub use chat::{
    ThreadSummary, attach_thread, create_thread, get_attached_thread, list_threads,
    receive_message, resolve_user,
};
pub use communities::{CommunitySummary, detect_communities};
pub use context::{BrainMessage, ContextBundle, ResolvedNode, select_context};
pub use embeddings::EmbeddingClient;
pub use facts::{contains_signal_phrases, extract_candidates};
pub use stack_backlog::{BacklogItem, BrainStateClient, StackItem};
pub use vault::{
    IndexReport, ParsedNode, VaultChunk, VaultConfig, chunk_markdown, extract_wikilinks,
    index_changed_files, index_vault, parse_frontmatter, parse_vault_file,
};
