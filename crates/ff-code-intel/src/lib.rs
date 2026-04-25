//! ForgeFleet Code Intelligence — AST parsing, structural search, and code knowledge graph.
//!
//! Provides agents with structural understanding of codebases beyond text search:
//! - Tree-sitter parsing for multi-language AST analysis
//! - Code entity extraction (functions, structs, classes, imports)
//! - Dependency graph construction (calls, imports, inheritance)
//! - Semantic compression for context-efficient code retrieval
//! - Distributed indexing across fleet nodes

pub mod graph;
pub mod index;
pub mod parser;
pub mod search;
