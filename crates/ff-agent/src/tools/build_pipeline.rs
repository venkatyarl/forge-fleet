//! Build pipeline tool — construct a cargo build pipeline that fetches
//! dependencies through the ForgeFleet local mirror first.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{AgentTool, AgentToolContext, AgentToolResult};

/// Agent tool that builds a `cargo check → build → test` pipeline.
///
/// When `FORGEFLEET_MIRROR_URL` is set (or the optional `mirror_url`
/// parameter is provided), a `mirror-fetch` step is inserted at the front
/// of the pipeline and all cargo steps are configured with cargo source
/// replacement so that every fetch is routed through the local mirror.
pub struct BuildPipelineTool;

#[async_trait]
impl AgentTool for BuildPipelineTool {
    fn name(&self) -> &str {
        "BuildPipeline"
    }

    fn description(&self) -> &str {
        "Construct a cargo check/build/test pipeline. When a mirror URL is available, a mirror-fetch step is inserted first so all dependency fetches go through the local mirror before building."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cwd": {
                    "type": "string",
                    "description": "Working directory for all pipeline commands"
                },
                "package": {
                    "type": "string",
                    "description": "Optional package to scope cargo commands to"
                },
                "mirror_url": {
                    "type": "string",
                    "description": "Optional ForgeFleet mirror URL. Defaults to FORGEFLEET_MIRROR_URL env var."
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let cwd = input.get("cwd").and_then(Value::as_str);
        let package = input.get("package").and_then(Value::as_str);
        let mirror_url = input
            .get("mirror_url")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .or_else(|| std::env::var("FORGEFLEET_MIRROR_URL").ok());

        let graph =
            ff_pipeline::templates::build_pipeline_with_mirror(cwd, package, mirror_url.as_deref());

        match serde_json::to_string_pretty(&graph) {
            Ok(json) => AgentToolResult::ok(json),
            Err(e) => AgentToolResult::err(format!("failed to serialize pipeline: {e}")),
        }
    }
}
