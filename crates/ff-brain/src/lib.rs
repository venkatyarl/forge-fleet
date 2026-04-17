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
pub mod vault;

pub use chat::{
    attach_thread, create_thread, get_attached_thread, list_threads, receive_message,
    resolve_user, ThreadSummary,
};
pub use communities::{detect_communities, CommunitySummary};
pub use context::{select_context, BrainMessage, ContextBundle, ResolvedNode};
pub use embeddings::EmbeddingClient;
pub use facts::{contains_signal_phrases, extract_candidates};
pub use vault::{
    chunk_markdown, extract_wikilinks, index_changed_files, index_vault, parse_frontmatter,
    parse_vault_file, IndexReport, ParsedNode, VaultChunk, VaultConfig,
};
