use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::store::{Memory, MemorySource, MemoryStore, NewMemory};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Fact,
    Decision,
    Preference,
    Summary,
}

impl CaptureKind {
    pub fn as_tag(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Decision => "decision",
            Self::Preference => "preference",
            Self::Summary => "summary",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptTurn {
    pub speaker: MemorySource,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureCandidate {
    pub kind: CaptureKind,
    pub speaker: MemorySource,
    pub content: String,
    pub tags: Vec<String>,
    pub importance: f32,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct AutoCaptureEngine {
    store: MemoryStore,
    pub max_captures_per_pass: usize,
}

impl AutoCaptureEngine {
    pub fn new(store: MemoryStore) -> Self {
        Self {
            store,
            max_captures_per_pass: 32,
        }
    }

    pub fn with_limit(mut self, max_captures_per_pass: usize) -> Self {
        self.max_captures_per_pass = max_captures_per_pass.max(1);
        self
    }

    pub fn extract_candidates(&self, transcript: &[TranscriptTurn]) -> Vec<CaptureCandidate> {
        let mut candidates = Vec::new();

        for turn in transcript {
            let text = normalize(&turn.content);
            if text.len() < 18 {
                continue;
            }

            let lower = text.to_ascii_lowercase();
            if let Some(kind) = classify_capture_kind(&lower) {
                let mut tags = vec!["auto_capture".to_string(), kind.as_tag().to_string()];
                if lower.contains("deadline") || lower.contains("due") {
                    tags.push("deadline".to_string());
                }
                if lower.contains("todo") || lower.contains("next") || lower.contains("follow up") {
                    tags.push("action_item".to_string());
                }

                candidates.push(CaptureCandidate {
                    kind,
                    speaker: turn.speaker,
                    importance: score_importance(&lower, kind),
                    content: text,
                    tags,
                    timestamp: turn.timestamp,
                });
            }
        }

        candidates.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.timestamp.cmp(&a.timestamp))
        });
        candidates.truncate(self.max_captures_per_pass);
        candidates
    }

    pub async fn capture_transcript(
        &self,
        workspace_id: &str,
        session_id: &str,
        transcript: &[TranscriptTurn],
    ) -> Result<Vec<Memory>> {
        let candidates = self.extract_candidates(transcript);
        let mut captured = Vec::with_capacity(candidates.len());

        for candidate in candidates {
            let mut tags = candidate.tags;
            tags.push(format!("session:{session_id}"));

            let memory = self
                .store
                .save_memory(NewMemory {
                    id: None,
                    workspace_id: workspace_id.to_string(),
                    content: candidate.content,
                    tags,
                    source: MemorySource::Session,
                    importance: Some(candidate.importance),
                    created_at: Some(candidate.timestamp),
                })
                .await?;
            captured.push(memory);
        }

        debug!(count = captured.len(), "auto-captured transcript memories");
        Ok(captured)
    }

    pub async fn compress_long_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        transcript: &[TranscriptTurn],
    ) -> Result<Option<Memory>> {
        if transcript.len() < 12 {
            return Ok(None);
        }

        let mut candidates = self.extract_candidates(transcript);
        if candidates.is_empty() {
            return Ok(None);
        }

        candidates.truncate(8);
        let summary = summarize_candidates(&candidates);

        let summary_memory = self
            .store
            .save_memory(NewMemory {
                id: None,
                workspace_id: workspace_id.to_string(),
                content: format!("Session {session_id} summary:\n{summary}"),
                tags: vec![
                    "session_summary".to_string(),
                    "auto_capture".to_string(),
                    format!("session:{session_id}"),
                ],
                source: MemorySource::Session,
                importance: Some(0.82),
                created_at: Some(Utc::now()),
            })
            .await?;

        Ok(Some(summary_memory))
    }
}

fn classify_capture_kind(lower: &str) -> Option<CaptureKind> {
    let decision_signals = ["decision", "decide", "we will", "ship", "final", "approved"];
    if decision_signals.iter().any(|s| lower.contains(s)) {
        return Some(CaptureKind::Decision);
    }

    let preference_signals = [
        "prefer",
        "i like",
        "i want",
        "always",
        "never",
        "don't",
        "should avoid",
    ];
    if preference_signals.iter().any(|s| lower.contains(s)) {
        return Some(CaptureKind::Preference);
    }

    let fact_signals = [
        "deadline",
        "due",
        "meeting",
        "project",
        "company",
        "customer",
        "production",
        "issue",
        "bug",
    ];
    if fact_signals.iter().any(|s| lower.contains(s)) {
        return Some(CaptureKind::Fact);
    }

    None
}

fn score_importance(lower: &str, kind: CaptureKind) -> f32 {
    let base: f32 = match kind {
        CaptureKind::Decision => 0.82,
        CaptureKind::Preference => 0.74,
        CaptureKind::Fact => 0.66,
        CaptureKind::Summary => 0.80,
    };

    let boosts: [(&str, f32); 7] = [
        ("urgent", 0.08),
        ("asap", 0.08),
        ("deadline", 0.1),
        ("production", 0.08),
        ("critical", 0.1),
        ("never", 0.05),
        ("always", 0.05),
    ];

    let mut score = base;
    for (needle, weight) in boosts {
        if lower.contains(needle) {
            score += weight;
        }
    }

    score.clamp(0.0, 1.0)
}

fn summarize_candidates(candidates: &[CaptureCandidate]) -> String {
    let mut lines = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let marker = match candidate.kind {
            CaptureKind::Decision => "Decision",
            CaptureKind::Preference => "Preference",
            CaptureKind::Fact => "Fact",
            CaptureKind::Summary => "Summary",
        };
        lines.push(format!("- [{marker}] {}", candidate.content));
    }
    lines.join("\n")
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
