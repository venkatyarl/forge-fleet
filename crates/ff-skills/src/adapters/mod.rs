//! Skill adapters — convert between ForgeFleet's universal skill format and
//! provider-specific formats (OpenClaw, Claude, MCP, custom).
//!
//! Each adapter implements the [`SkillAdapter`] trait, providing bidirectional
//! conversion: import (foreign → universal) and export (universal → foreign).

pub mod claude;
pub mod custom;
pub mod mcp;
pub mod openclaw;

use crate::error::Result;
use crate::types::SkillMetadata;

/// A skill adapter converts between ForgeFleet's universal skill model and a
/// provider-specific format.
///
/// Adapters are stateless — they work on data, not connections.
pub trait SkillAdapter: Send + Sync {
    /// Adapter name (e.g. "openclaw", "claude", "mcp").
    fn name(&self) -> &str;

    /// Import: convert a provider-specific JSON payload into universal metadata.
    fn import(&self, raw: &serde_json::Value) -> Result<SkillMetadata>;

    /// Export: convert universal metadata into the provider-specific format.
    fn export(&self, skill: &SkillMetadata) -> Result<serde_json::Value>;

    /// Check whether this adapter can handle the given raw payload.
    ///
    /// Used for auto-detection when the origin is unknown.
    fn can_handle(&self, raw: &serde_json::Value) -> bool;
}
