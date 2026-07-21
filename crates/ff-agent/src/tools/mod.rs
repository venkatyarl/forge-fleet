//! Agent tool system — typed tools with async execution for the ForgeFleet agent loop.
//!
//! Provides a trait-based tool framework for LLM-driven task execution,
//! designed for ForgeFleet's distributed fleet with OpenAI-compatible LLMs.

pub mod agent_tool;
pub mod agentic;
pub mod analytics;
pub mod bash;
pub mod build_pipeline;
pub mod code_quality;
pub mod computer;
pub mod content;
pub mod cron_tool;
pub mod crypto;
pub mod database;
pub mod dep_check;
pub mod diff_tool;
pub mod doc_gen;
pub mod docker_manage;
pub mod env_info;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod finance;
pub mod fleet_ops;
pub mod git_pr;
pub mod git_tools;
pub mod glob_tool;
pub mod grep_tool;
pub mod http_request;
pub mod intelligence;
pub mod json_query;
pub mod lint_fix;
pub mod media;
pub mod mlx_degraded;
pub mod model_discovery;
pub mod model_mgmt;
pub mod multimodal;
pub mod network_check;
pub mod notebook_edit;
pub mod orchestrate;
pub mod plan_tools;
pub mod pr_review;
pub mod project_mgmt;
pub mod research;
pub mod research_ext;
pub mod send_message;
pub mod skill_builder;
pub mod sleep_tool;
pub mod software_detect;
pub mod software_upgrade;
pub mod task_tools;
pub mod tool_builder;
pub mod tool_search;
pub mod training_tool;
pub mod uiux;
pub mod utility_ext;
pub mod version_mgmt;
pub mod web_fetch;
pub mod web_search;
pub mod work_item_tool;
pub mod worktree;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::Value;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Shared HTTP client for tools
// ---------------------------------------------------------------------------
//
// Several tools (SendMessage, HealthMonitor, LinkPreview, VideoAnalyze) used
// to construct their own `reqwest::Client` in `Default::default()`. Each
// instance carried its own DNS resolver, TLS state, and connection pool.
// Because tool instances are created per agent and discarded between turns,
// these clients churned heavily under load.
//
// `reqwest::Client` is already internally `Arc<Inner>` — cloning is cheap, so
// one shared client serves every tool. Per-request timeouts (set via
// `RequestBuilder::timeout`) override the client default, so call sites that
// care about a specific deadline still set their own.
static SHARED_HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent("ForgeFleet-Agent/1.0")
        .build()
        .expect("build shared tool reqwest client")
});

/// Returns a shared `reqwest::Client` used by all tools that need HTTP.
/// Cloning the result is cheap (internal Arc).
pub fn shared_http_client() -> reqwest::Client {
    SHARED_HTTP_CLIENT.clone()
}

// ---------------------------------------------------------------------------
// Core trait
// ---------------------------------------------------------------------------

/// A tool that can be invoked by the agent loop during LLM-driven execution.
#[async_trait]
pub trait AgentTool: Send + Sync {
    /// Tool name as it appears in OpenAI function-calling (e.g. "Bash", "Read").
    fn name(&self) -> &str;

    /// Human-readable description sent to the LLM.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's input parameters.
    fn parameters_schema(&self) -> Value;

    /// Execute the tool with the given JSON input and return a result.
    async fn execute(&self, input: Value, ctx: &AgentToolContext) -> AgentToolResult;
}

// ---------------------------------------------------------------------------
// Context & Result
// ---------------------------------------------------------------------------

/// Shared context available to all tools during execution.
#[derive(Clone)]
pub struct AgentToolContext {
    /// Working directory for file operations and shell commands.
    pub working_dir: PathBuf,
    /// Unique session identifier.
    pub session_id: String,
    /// Persistent shell state (cwd + env vars) across Bash invocations.
    pub shell_state: Arc<Mutex<ShellState>>,
    /// Serializes file mutations (Edit/Write/NotebookEdit) within this checkout.
    /// Multiple sessions can share a worktree during one build, so the lock is
    /// keyed by working directory rather than session ID.
    pub edit_lock: Arc<Mutex<()>>,
    /// Optional Postgres pool for audit logging and security checks.
    pub pg_pool: Option<sqlx::PgPool>,
}

/// Persistent shell state across multiple Bash tool invocations within a session.
#[derive(Debug, Clone, Default)]
pub struct ShellState {
    /// Current working directory (set by `cd` commands).
    pub cwd: Option<PathBuf>,
    /// Environment variables set by `export` commands.
    pub env_vars: HashMap<String, String>,
}

/// Result of a tool execution.
#[derive(Debug, Clone)]
pub struct AgentToolResult {
    /// Output content returned to the LLM.
    pub content: String,
    /// Whether the execution resulted in an error.
    pub is_error: bool,
    /// If true, the agent loop should terminate this turn immediately after
    /// this tool call — do NOT run the LLM again. Used by `AskUserQuestion`
    /// so the agent doesn't rationalize past the question and keep looping.
    /// The next user input is expected to be the answer.
    pub should_end_turn: bool,
}

impl AgentToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            should_end_turn: false,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            should_end_turn: false,
        }
    }

    /// Mark this result as terminating the current agent turn. Next user
    /// message will be treated as a fresh turn (or the answer to an
    /// outstanding AskUserQuestion).
    pub fn end_turn(mut self) -> Self {
        self.should_end_turn = true;
        self
    }
}

// ---------------------------------------------------------------------------
// Process-global shell state registry
// ---------------------------------------------------------------------------

/// Global registry of shell states keyed by session ID, so each session
/// maintains its own persistent cwd and env vars across Bash calls.
static SHELL_STATES: std::sync::LazyLock<DashMap<String, Arc<Mutex<ShellState>>>> =
    std::sync::LazyLock::new(DashMap::new);

/// Get or create a shell state for the given session.
pub fn session_shell_state(session_id: &str) -> Arc<Mutex<ShellState>> {
    SHELL_STATES
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(ShellState::default())))
        .clone()
}

/// Clear the shell state for a session (on session end).
pub fn clear_session_shell_state(session_id: &str) {
    SHELL_STATES.remove(session_id);
}

/// Global registry of per-checkout edit locks. Weak references avoid retaining
/// every checkout path seen by a long-running agent after its sessions finish.
static EDIT_LOCKS: std::sync::LazyLock<DashMap<PathBuf, std::sync::Weak<Mutex<()>>>> =
    std::sync::LazyLock::new(DashMap::new);

/// Get or create the edit-serialization lock for a checkout.
pub fn checkout_edit_lock(working_dir: &std::path::Path) -> Arc<Mutex<()>> {
    let key = std::fs::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
    EDIT_LOCKS.retain(|_, lock| lock.strong_count() > 0);
    let mut entry = EDIT_LOCKS.entry(key).or_default();
    if let Some(lock) = entry.upgrade() {
        return lock;
    }

    let lock = Arc::new(Mutex::new(()));
    *entry = Arc::downgrade(&lock);
    lock
}

// ---------------------------------------------------------------------------
// Tool registry helpers
// ---------------------------------------------------------------------------

/// Returns only core tools (sent to LLM by default — keeps prompt small).
/// Other tools are loaded on demand via ToolSearch.
pub fn core_tools() -> Vec<Box<dyn AgentTool>> {
    vec![
        Box::new(bash::BashTool),
        Box::new(file_read::FileReadTool),
        Box::new(file_write::FileWriteTool),
        Box::new(file_edit::FileEditTool),
        Box::new(glob_tool::GlobTool),
        Box::new(grep_tool::GrepTool),
        Box::new(agent_tool::SubAgentTool),
        Box::new(web_fetch::WebFetchTool::default()),
        Box::new(web_search::WebSearchTool::default()),
        Box::new(plan_tools::AskUserQuestionTool),
        Box::new(orchestrate::OrchestrateTool),
        Box::new(tool_search::ToolSearchTool),
        Box::new(training_tool::TrainingTool),
        Box::new(work_item_tool::ListWorkItemsTool),
    ]
}

/// Returns core tools as Arc (for parallel execution).
pub fn core_tools_arc() -> Vec<Arc<dyn AgentTool>> {
    core_tools().into_iter().map(Arc::from).collect()
}

/// Returns all built-in agent tools (boxed) — used for ToolSearch discovery.
pub fn all_tools() -> Vec<Box<dyn AgentTool>> {
    vec![
        // Core file & shell tools
        Box::new(bash::BashTool),
        Box::new(file_read::FileReadTool),
        Box::new(file_write::FileWriteTool),
        Box::new(file_edit::FileEditTool),
        Box::new(glob_tool::GlobTool),
        Box::new(grep_tool::GrepTool),
        // Agent & coordination tools
        Box::new(agent_tool::SubAgentTool),
        Box::new(build_pipeline::BuildPipelineTool),
        Box::new(send_message::SendMessageTool::default()),
        // Task management tools
        Box::new(task_tools::TaskCreateTool),
        Box::new(task_tools::TaskGetTool),
        Box::new(task_tools::TaskUpdateTool::default()),
        Box::new(task_tools::TaskListTool),
        Box::new(task_tools::TaskStopTool),
        Box::new(task_tools::TaskOutputTool),
        Box::new(work_item_tool::ListWorkItemsTool),
        // Web tools
        Box::new(web_fetch::WebFetchTool::default()),
        Box::new(web_search::WebSearchTool::default()),
        // Planning tools
        Box::new(plan_tools::AskUserQuestionTool),
        Box::new(orchestrate::OrchestrateTool),
        Box::new(plan_tools::EnterPlanModeTool),
        Box::new(plan_tools::ExitPlanModeTool),
        // Git & utility tools
        Box::new(worktree::EnterWorktreeTool),
        Box::new(worktree::ExitWorktreeTool),
        Box::new(sleep_tool::SleepTool),
        Box::new(notebook_edit::NotebookEditTool),
        // Git tools
        Box::new(git_tools::GitBlameTool),
        Box::new(git_tools::TestGenTool),
        Box::new(git_pr::GitPRTool),
        Box::new(pr_review::PrReviewTool),
        // DevOps tools
        Box::new(docker_manage::DockerManageTool),
        Box::new(lint_fix::LintFixTool),
        Box::new(doc_gen::DocGenTool),
        Box::new(dep_check::DepCheckTool),
        Box::new(cron_tool::CronScheduleTool),
        // Utility tools
        Box::new(http_request::HttpRequestTool::default()),
        Box::new(diff_tool::DiffTool),
        Box::new(json_query::JsonQueryTool),
        Box::new(env_info::EnvInfoTool),
        Box::new(network_check::NetworkCheckTool::default()),
        // Research tools
        Box::new(research::DeepResearchTool),
        Box::new(research::WikiLookupTool::default()),
        Box::new(research::ScholarSearchTool::default()),
        // Agentic tools
        Box::new(agentic::VerifyAndRetryTool),
        Box::new(agentic::DelegateTool),
        Box::new(agentic::PdfExtractTool),
        Box::new(agentic::SpreadsheetQueryTool),
        // Project management tools
        Box::new(project_mgmt::ProjectEstimateTool),
        Box::new(project_mgmt::VelocityTrackerTool),
        Box::new(project_mgmt::DeadlineProjectorTool),
        Box::new(project_mgmt::SprintPlannerTool),
        Box::new(project_mgmt::RiskAssessorTool),
        Box::new(project_mgmt::WorkloadBalancerTool),
        Box::new(project_mgmt::DependencyMapperTool),
        // Finance tools
        Box::new(finance::BudgetTrackerTool),
        Box::new(finance::ProfitLossTool),
        Box::new(finance::CashFlowForecastTool),
        Box::new(finance::InvoiceGenTool),
        // Analytics tools
        Box::new(analytics::StatsCalcTool),
        Box::new(analytics::TimeSeriesAnalysisTool),
        // Content tools
        Box::new(content::ChangelogGenTool),
        Box::new(content::ReportGenTool),
        Box::new(content::MeetingNotesTool),
        // Code quality tools
        Box::new(code_quality::CodeComplexityTool),
        Box::new(code_quality::DuplicateDetectorTool),
        Box::new(code_quality::LogAnalyzerTool),
        // Fleet operations tools
        Box::new(fleet_ops::NodeSetupTool),
        Box::new(fleet_ops::NodeEnrollTool::default()),
        Box::new(fleet_ops::ModelDeployTool),
        Box::new(fleet_ops::FleetInventoryTool::default()),
        Box::new(fleet_ops::NodeHealthCheckTool::default()),
        Box::new(mlx_degraded::MlxDegradedTool),
        Box::new(fleet_ops::BinaryDeployTool),
        // Intelligence & self-improvement tools
        Box::new(intelligence::PatternLearnerTool),
        Box::new(intelligence::ModelScorecardTool),
        Box::new(intelligence::ReviewQueueTool),
        Box::new(intelligence::RollbackManagerTool),
        Box::new(intelligence::SmartSearchTool),
        Box::new(intelligence::WatchAndReactTool),
        Box::new(intelligence::ProjectScaffoldTool),
        // Media tools
        Box::new(media::ScreenshotCaptureTool),
        Box::new(media::ImageAnalyzeTool),
        Box::new(media::VideoDownloadTool),
        Box::new(media::VideoAnalyzeTool::default()),
        Box::new(media::AudioAnalyzeTool),
        Box::new(media::LinkPreviewTool::default()),
        Box::new(media::ImageConvertTool),
        // Skill builder
        Box::new(skill_builder::SkillBuilderTool::default()),
        // Tool builder
        Box::new(tool_builder::ToolBuilderTool),
        // Computer tools
        Box::new(computer::ProcessManagerTool),
        Box::new(computer::ClipboardTool),
        Box::new(computer::SystemControlTool),
        Box::new(computer::ServiceManagerTool),
        Box::new(computer::PackageManagerTool),
        // Database & crypto tools
        Box::new(database::DatabaseQueryTool),
        Box::new(crypto::HashGeneratorTool),
        Box::new(crypto::PasswordGenTool),
        Box::new(crypto::TextTransformTool),
        Box::new(crypto::CalculatorTool),
        // Model management tools
        Box::new(model_mgmt::ModelBrowserTool::default()),
        Box::new(model_mgmt::ModelDownloaderTool),
        Box::new(model_mgmt::ModelCompareTool),
        // Version management
        Box::new(version_mgmt::VersionManagerTool),
        // Model discovery & clustering
        Box::new(model_discovery::ModelDiscoveryTool::default()),
        Box::new(model_discovery::ClusterInferenceTool::default()),
        // Extended utility tools
        Box::new(utility_ext::ReminderTool),
        Box::new(utility_ext::TimerTool),
        Box::new(utility_ext::TimezoneConvertTool),
        Box::new(utility_ext::RegexTool),
        Box::new(utility_ext::DiagramTool),
        Box::new(utility_ext::TranslateTool),
        Box::new(utility_ext::FileCompressTool),
        Box::new(utility_ext::FileSyncTool),
        Box::new(utility_ext::HealthMonitorTool::default()),
        Box::new(utility_ext::GithubIssuesTool),
        Box::new(utility_ext::MarkdownTool),
        // Multimodal tools
        Box::new(multimodal::PhotoAnalysisTool),
        Box::new(multimodal::VideoAnalysisTool),
        Box::new(multimodal::AudioAnalysisTool),
        // Self-healing & fleet automation
        Box::new(multimodal::SelfHealTool::default()),
        Box::new(multimodal::AutoFleetTool::default()),
        Box::new(multimodal::TaskDecomposerTool),
        // UI/UX tools
        Box::new(uiux::ColorPaletteTool),
        Box::new(uiux::AccessibilityCheckTool),
        Box::new(uiux::ComponentScaffoldTool),
        Box::new(uiux::ResponsiveTestTool),
        Box::new(uiux::CSSAnalyzerTool),
        Box::new(uiux::StyleGuideGenTool),
        // Extended research tools
        Box::new(research_ext::CompetitorAnalysisTool),
        Box::new(research_ext::TrendAnalysisTool::default()),
        Box::new(research_ext::MarketResearchTool),
    ]
}

/// Returns all built-in agent tools as Arc (for parallel execution).
pub fn all_tools_arc() -> Vec<Arc<dyn AgentTool>> {
    all_tools().into_iter().map(Arc::from).collect()
}

/// Find a tool by name (case-insensitive).
pub fn find_tool(name: &str, tools: &[Box<dyn AgentTool>]) -> Option<usize> {
    let lower = name.to_ascii_lowercase();
    tools
        .iter()
        .position(|t| t.name().to_ascii_lowercase() == lower)
}

/// Find a tool by name in Arc-based tool list.
pub fn find_tool_arc(name: &str, tools: &[Arc<dyn AgentTool>]) -> Option<usize> {
    let lower = name.to_ascii_lowercase();
    tools
        .iter()
        .position(|t| t.name().to_ascii_lowercase() == lower)
}

/// Maximum tool result size in characters before truncation.
pub const MAX_TOOL_RESULT_CHARS: usize = 16_384;

/// POSIX shell single-quote escape: wraps the argument in single quotes and
/// escapes any embedded single quotes. Safe for pasting into `sh -c`.
pub fn shell_escape_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Truncate output to a safe UTF-8 boundary with a marker.
pub fn truncate_output(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }
    let mut end = max_chars;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}...\n[truncated — {} total chars]",
        &output[..end],
        output.len()
    )
}
