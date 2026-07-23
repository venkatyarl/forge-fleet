//! `ff-memory` — ForgeFleet universal memory layer.
//!
//! This crate provides:
//! - Durable long-term memory storage (Postgres)
//! - Automatic fact/decision/preference capture from transcripts
//! - Retrieval and ranking by keyword, recency, and importance
//! - Document ingestion and lightweight keyword RAG
//! - Per-session working memory with promotion to long-term memory
//! - Workspace-level memory isolation and cross-workspace search

pub mod cache;
pub mod capture;
pub mod graph;
pub mod rag;
pub mod retrieval;
pub mod session;
pub mod store;
pub mod types;
pub mod user_model;
pub mod workspace;

pub use cache::{ContextCache, WorkItemContextCache};
pub use capture::{AutoCaptureEngine, CaptureCandidate, CaptureKind, TranscriptTurn};
pub use graph::{Edge, GraphScope, Subgraph};
pub use rag::{DocumentKind, IngestReport, RagChunk, RagEngine, RagQuery, RagResult};
pub use retrieval::{MemoryRetrievalEngine, RetrievalQuery, RetrievalResult};
pub use session::{SessionClosureResult, SessionMemoryItem, SessionMemoryManager};
pub use store::{
    Memory, MemorySource, MemoryStore, MemoryStoreError, NewMemory, SearchMemoriesParams,
};
pub use types::{EdgeType, NodeId, Realm, RealmId};
pub use user_model::{
    CrossRealmEdge, CrossRealmEdgeError, UserModel, UserModelContext, UserModelNode,
    validate_cross_realm,
};
pub use workspace::{
    WorkspaceMemoryManager, WorkspaceProfile, WorkspaceScopedStore, WorkspaceSearchHit,
};
