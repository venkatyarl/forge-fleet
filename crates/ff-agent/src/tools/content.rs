//! Content & communication tools — email drafts, reports, changelogs, meeting notes.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{AgentTool, AgentToolContext, AgentToolResult, MAX_TOOL_RESULT_CHARS, truncate_output};

pub struct ChangelogGenTool;
#[async_trait]
impl AgentTool for ChangelogGenTool {
    fn name(&self) -> &str {
        "ChangelogGen"
    }
    fn description(&self) -> &str {
        "Generate a changelog from git history. Groups commits by type (feat, fix, refactor, etc.)."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"since":{"type":"string","description":"Git ref to start from (e.g. 'v0.1.0', 'HEAD~20', '2026-04-01')"},"format":{"type":"string","enum":["markdown","plain"],"description":"Output format"}}})
    }
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult {
        let since = input
            .get("since")
            .and_then(Value::as_str)
            .unwrap_or("HEAD~20");
        let output = Command::new("git")
            .args(["log", "--oneline", &format!("{since}..HEAD")])
            .current_dir(&ctx.working_dir)
            .output()
            .await;
        match output {
            Ok(out) if out.status.success() => {
                let commits = String::from_utf8_lossy(&out.stdout);
                let mut feats = Vec::new();
                let mut fixes = Vec::new();
                let mut others = Vec::new();
                for line in commits.lines() {
                    let msg = line.split_once(' ').map(|(_, m)| m).unwrap_or(line);
                    if msg.starts_with("feat") {
                        feats.push(format!("- {msg}"));
                    } else if msg.starts_with("fix") {
                        fixes.push(format!("- {msg}"));
                    } else {
                        others.push(format!("- {msg}"));
                    }
                }
                let mut cl = String::from("# Changelog\n\n");
                if !feats.is_empty() {
                    cl.push_str(&format!("## Features\n{}\n\n", feats.join("\n")));
                }
                if !fixes.is_empty() {
                    cl.push_str(&format!("## Fixes\n{}\n\n", fixes.join("\n")));
                }
                if !others.is_empty() {
                    cl.push_str(&format!("## Other\n{}\n\n", others.join("\n")));
                }
                AgentToolResult::ok(truncate_output(&cl, MAX_TOOL_RESULT_CHARS))
            }
            _ => AgentToolResult::err("Failed to read git log".to_string()),
        }
    }
}

pub struct ReportGenTool;
#[async_trait]
impl AgentTool for ReportGenTool {
    fn name(&self) -> &str {
        "ReportGen"
    }
    fn description(&self) -> &str {
        "Generate a structured report in markdown from sections and data."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"title":{"type":"string"},"sections":{"type":"array","items":{"type":"object","properties":{"heading":{"type":"string"},"content":{"type":"string"}}}},"summary":{"type":"string"}},"required":["title","sections"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let title = input
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Report");
        let sections = input
            .get("sections")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let summary = input.get("summary").and_then(Value::as_str).unwrap_or("");
        let date = chrono::Utc::now().format("%B %d, %Y");
        let mut report = format!("# {title}\n\n**Date:** {date}\n\n");
        if !summary.is_empty() {
            report.push_str(&format!("## Summary\n\n{summary}\n\n"));
        }
        for section in &sections {
            let heading = section
                .get("heading")
                .and_then(Value::as_str)
                .unwrap_or("Section");
            let content = section.get("content").and_then(Value::as_str).unwrap_or("");
            report.push_str(&format!("## {heading}\n\n{content}\n\n"));
        }
        AgentToolResult::ok(report)
    }
}

pub struct MeetingNotesTool;
#[async_trait]
impl AgentTool for MeetingNotesTool {
    fn name(&self) -> &str {
        "MeetingNotes"
    }
    fn description(&self) -> &str {
        "Structure meeting notes with attendees, topics, decisions, and action items."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{"title":{"type":"string"},"attendees":{"type":"array","items":{"type":"string"}},"raw_notes":{"type":"string","description":"Unstructured meeting notes to organize"}},"required":["raw_notes"]})
    }
    async fn execute(&self, input: Value, _ctx: &AgentToolContext) -> AgentToolResult {
        let title = input
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Meeting Notes");
        let attendees: Vec<&str> = input
            .get("attendees")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let notes = input.get("raw_notes").and_then(Value::as_str).unwrap_or("");
        let date = chrono::Utc::now().format("%B %d, %Y");

        let mut output = format!("# {title}\n\n**Date:** {date}\n");
        if !attendees.is_empty() {
            output.push_str(&format!("**Attendees:** {}\n", attendees.join(", ")));
        }
        output.push_str(&format!("\n## Notes\n\n{notes}\n\n## Action Items\n\n(Extract action items from the notes above)\n\n## Decisions\n\n(Extract decisions from the notes above)\n"));
        AgentToolResult::ok(output)
    }
}
