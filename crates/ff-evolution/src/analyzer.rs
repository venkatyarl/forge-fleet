//! Failure analysis for evolution loops.
//!
//! Turns raw build/test/runtime failure signals into categorized root causes.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Origin of a failure observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSource {
    Build,
    Test,
    Runtime,
    Infrastructure,
    Unknown,
}

/// High-level failure class used for repair routing and reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    Build,
    Test,
    Runtime,
    Infrastructure,
    Flaky,
    Unknown,
}

/// Fine-grained root cause categories extracted from logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootCauseCategory {
    CompilationError,
    DependencyResolution,
    MissingConfiguration,
    ApiContractMismatch,
    NetworkInstability,
    ResourceExhaustion,
    TestRegression,
    FlakyBehavior,
    ToolingFailure,
    Unknown,
}

/// Raw signal the loop observed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureObservation {
    pub id: Uuid,
    pub source: FailureSource,
    pub summary: String,
    pub log: String,
    pub metadata: serde_json::Value,
    pub observed_at: DateTime<Utc>,
}

impl FailureObservation {
    pub fn new(source: FailureSource, summary: impl Into<String>, log: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            source,
            summary: summary.into(),
            log: log.into(),
            metadata: serde_json::json!({}),
            observed_at: Utc::now(),
        }
    }
}

/// A concrete, scored root-cause hypothesis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootCause {
    pub id: Uuid,
    pub category: RootCauseCategory,
    pub failure_category: FailureCategory,
    pub summary: String,
    pub evidence: Vec<String>,
    pub confidence: f32,
    pub fingerprint: String,
    pub created_at: DateTime<Utc>,
}

/// Full output from an analyzer pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub id: Uuid,
    pub observation_id: Uuid,
    pub failure_category: FailureCategory,
    pub causes: Vec<RootCause>,
    pub primary: Option<RootCause>,
    pub analyzed_at: DateTime<Utc>,
    pub classifier_version: String,
}

impl AnalysisReport {
    pub fn has_actionable_causes(&self) -> bool {
        self.causes
            .iter()
            .any(|cause| cause.category != RootCauseCategory::Unknown)
    }
}

#[derive(Debug, Clone)]
struct Rule {
    category: RootCauseCategory,
    failure_category: FailureCategory,
    keywords: Vec<&'static str>,
    weight: f32,
    summary: &'static str,
}

/// Rule-based analyzer for build/test/runtime logs.
#[derive(Debug, Clone)]
pub struct FailureAnalyzer {
    rules: Vec<Rule>,
    pub classifier_version: String,
}

impl Default for FailureAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FailureAnalyzer {
    pub fn new() -> Self {
        let rules = vec![
            Rule {
                category: RootCauseCategory::CompilationError,
                failure_category: FailureCategory::Build,
                keywords: vec![
                    "error:",
                    "cannot find",
                    "mismatched types",
                    "failed to compile",
                    "use of undeclared",
                ],
                weight: 0.92,
                summary: "Compilation failure in source or generated code",
            },
            Rule {
                category: RootCauseCategory::DependencyResolution,
                failure_category: FailureCategory::Build,
                keywords: vec![
                    "failed to select a version",
                    "version solving failed",
                    "could not resolve",
                    "checksum",
                    "cargo update",
                ],
                weight: 0.88,
                summary: "Dependency resolution conflict or lockfile drift",
            },
            Rule {
                category: RootCauseCategory::MissingConfiguration,
                failure_category: FailureCategory::Infrastructure,
                keywords: vec![
                    "missing environment variable",
                    "no such file or directory",
                    "permission denied",
                    "not found in configuration",
                    "invalid configuration",
                ],
                weight: 0.83,
                summary: "Configuration or environment setup is incomplete",
            },
            Rule {
                category: RootCauseCategory::ApiContractMismatch,
                failure_category: FailureCategory::Runtime,
                keywords: vec![
                    "unexpected status",
                    "invalid response",
                    "deserialization error",
                    "schema mismatch",
                    "unsupported media type",
                ],
                weight: 0.79,
                summary: "API contract mismatch between caller and dependency",
            },
            Rule {
                category: RootCauseCategory::NetworkInstability,
                failure_category: FailureCategory::Infrastructure,
                keywords: vec![
                    "connection refused",
                    "connection reset",
                    "timed out",
                    "dns",
                    "temporary failure",
                ],
                weight: 0.76,
                summary: "Network connectivity instability",
            },
            Rule {
                category: RootCauseCategory::ResourceExhaustion,
                failure_category: FailureCategory::Infrastructure,
                keywords: vec![
                    "out of memory",
                    "oom",
                    "disk quota exceeded",
                    "too many open files",
                    "resource temporarily unavailable",
                ],
                weight: 0.87,
                summary: "Resource exhaustion on node or runtime",
            },
            Rule {
                category: RootCauseCategory::TestRegression,
                failure_category: FailureCategory::Test,
                keywords: vec![
                    "assertion failed",
                    "expected",
                    "snapshot mismatch",
                    "test failed",
                    "panicked at",
                ],
                weight: 0.9,
                summary: "Behavioral regression detected by tests",
            },
            Rule {
                category: RootCauseCategory::FlakyBehavior,
                failure_category: FailureCategory::Flaky,
                keywords: vec![
                    "flaky",
                    "intermittent",
                    "retry passed",
                    "non-deterministic",
                    "race condition",
                ],
                weight: 0.8,
                summary: "Intermittent or non-deterministic behavior",
            },
            Rule {
                category: RootCauseCategory::ToolingFailure,
                failure_category: FailureCategory::Build,
                keywords: vec![
                    "tool exited with code",
                    "cargo metadata",
                    "rustfmt",
                    "clippy",
                    "runner failed",
                ],
                weight: 0.68,
                summary: "Build toolchain or CI runner issue",
            },
        ];

        Self {
            rules,
            classifier_version: "ff-evolution-rule-v1".to_string(),
        }
    }

    /// Analyze a single observation into ranked root causes.
    pub fn analyze(&self, observation: &FailureObservation) -> AnalysisReport {
        let text = format!("{}\n{}", observation.summary, observation.log).to_lowercase();
        let inferred_category = self.infer_failure_category(observation.source, &text);

        let mut causes = Vec::new();
        for rule in &self.rules {
            let hits: Vec<&str> = rule
                .keywords
                .iter()
                .copied()
                .filter(|keyword| text.contains(keyword))
                .collect();

            if hits.is_empty() {
                continue;
            }

            let keyword_coverage = hits.len() as f32 / rule.keywords.len() as f32;
            let confidence = (rule.weight * 0.7) + (keyword_coverage * 0.3);
            let evidence: Vec<String> = hits.iter().map(|h| (*h).to_string()).collect();
            let fingerprint =
                build_fingerprint(observation.source, rule.category, &evidence, &text);

            causes.push(RootCause {
                id: Uuid::new_v4(),
                category: rule.category,
                failure_category: if rule.failure_category == FailureCategory::Unknown {
                    inferred_category
                } else {
                    rule.failure_category
                },
                summary: rule.summary.to_string(),
                evidence,
                confidence: confidence.clamp(0.05, 0.99),
                fingerprint,
                created_at: Utc::now(),
            });
        }

        if causes.is_empty() {
            causes.push(RootCause {
                id: Uuid::new_v4(),
                category: RootCauseCategory::Unknown,
                failure_category: inferred_category,
                summary: "No rule matched. Requires manual triage or a new analyzer rule."
                    .to_string(),
                evidence: vec![observation.summary.chars().take(120).collect()],
                confidence: 0.2,
                fingerprint: build_fingerprint(
                    observation.source,
                    RootCauseCategory::Unknown,
                    &[],
                    &text,
                ),
                created_at: Utc::now(),
            });
        }

        causes.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        let primary = causes.first().cloned();

        AnalysisReport {
            id: Uuid::new_v4(),
            observation_id: observation.id,
            failure_category: inferred_category,
            causes,
            primary,
            analyzed_at: Utc::now(),
            classifier_version: self.classifier_version.clone(),
        }
    }

    fn infer_failure_category(&self, source: FailureSource, text: &str) -> FailureCategory {
        match source {
            FailureSource::Build => FailureCategory::Build,
            FailureSource::Test => {
                if text.contains("flaky") || text.contains("retry passed") {
                    FailureCategory::Flaky
                } else {
                    FailureCategory::Test
                }
            }
            FailureSource::Runtime => FailureCategory::Runtime,
            FailureSource::Infrastructure => FailureCategory::Infrastructure,
            FailureSource::Unknown => {
                if text.contains("assertion failed") || text.contains("test failed") {
                    FailureCategory::Test
                } else if text.contains("failed to compile") || text.contains("mismatched types") {
                    FailureCategory::Build
                } else if text.contains("connection") || text.contains("timed out") {
                    FailureCategory::Infrastructure
                } else {
                    FailureCategory::Unknown
                }
            }
        }
    }
}

fn build_fingerprint(
    source: FailureSource,
    category: RootCauseCategory,
    evidence: &[String],
    text: &str,
) -> String {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    category.hash(&mut hasher);
    evidence.hash(&mut hasher);

    // Include a stable prefix of text so similar failures share fingerprint,
    // but avoid storing full logs in the key.
    let prefix: String = text.chars().take(200).collect();
    prefix.hash(&mut hasher);

    format!("{:x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_build_compilation_error() {
        let analyzer = FailureAnalyzer::new();
        let obs = FailureObservation::new(
            FailureSource::Build,
            "cargo check failed",
            "error: cannot find type `LoopState` in this scope\nfailed to compile ff-evolution",
        );

        let report = analyzer.analyze(&obs);
        let primary = report.primary.expect("expected primary root cause");

        assert_eq!(report.failure_category, FailureCategory::Build);
        assert_eq!(primary.category, RootCauseCategory::CompilationError);
        assert!(primary.confidence > 0.5);
    }

    #[test]
    fn detects_flaky_test_behavior() {
        let analyzer = FailureAnalyzer::new();
        let obs = FailureObservation::new(
            FailureSource::Test,
            "ci tests intermittently fail",
            "test failed on first run, retry passed; marked flaky due to race condition",
        );

        let report = analyzer.analyze(&obs);
        assert_eq!(report.failure_category, FailureCategory::Flaky);
        assert!(
            report
                .causes
                .iter()
                .any(|cause| cause.category == RootCauseCategory::FlakyBehavior)
        );
    }

    #[test]
    fn falls_back_to_unknown_when_no_rule_matches() {
        let analyzer = FailureAnalyzer::new();
        let obs = FailureObservation::new(
            FailureSource::Unknown,
            "something weird happened",
            "zarg blip blorp with no known markers",
        );

        let report = analyzer.analyze(&obs);
        assert_eq!(report.causes.len(), 1);
        assert_eq!(report.causes[0].category, RootCauseCategory::Unknown);
        assert!(!report.has_actionable_causes());
    }
}
