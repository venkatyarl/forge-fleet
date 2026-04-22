//! App state — the central state container for ForgeFleet Terminal.

use std::path::PathBuf;

use ff_agent::agent_loop::{AgentEvent, AgentSession, AgentSessionConfig};
use ff_agent::commands::CommandRegistry;
use ff_agent::focus_stack::{ConversationTracker, BacklogPriority, PushReason};

use crate::input::InputState;
use crate::messages::{DisplayMessage, render_user_message};

// ─── Port scheme (same on every node) ──────────────────────────────────────

/// ForgeFleet daemon port
pub const PORT_DAEMON: u16 = 51000;
/// LLM inference API port
pub const PORT_LLM: u16 = 51001;
/// Web UI port
pub const PORT_WEB: u16 = 51002;
/// WebSocket port
pub const PORT_WS: u16 = 51003;
/// Metrics/Prometheus port
pub const PORT_METRICS: u16 = 51004;

// ─── Main app state ────────────────────────────────────────────────────────

pub struct App {
    // Config
    pub config: AgentSessionConfig,
    pub commands: CommandRegistry,

    // Tabs — multiple sessions in the same TUI
    pub tabs: Vec<SessionTab>,
    pub active_tab: usize,

    // Global state
    pub frame: u64,
    pub should_quit: bool,
    pub fleet_nodes: Vec<FleetNode>,
    pub current_project: Option<ProjectInfo>,
    pub working_dir: PathBuf,
    pub brain_status: Option<ff_agent::brain::BrainLoadedStatus>,

    /// Active modal overlay (e.g. model picker). When Some, key input is captured by the overlay.
    pub picker: Option<ModelPicker>,
}

/// Interactive model picker overlay shown when user runs `/model` with no args.
#[derive(Debug, Clone, Default)]
pub struct ModelPicker {
    /// All models loaded from the fleet DB (deduplicated by name, with node list).
    pub items: Vec<ModelPickerItem>,
    /// True until the async load completes.
    pub loading: bool,
    /// Optional load error to display.
    pub error: Option<String>,
    /// Currently highlighted index in the *filtered* view.
    pub selected: usize,
    /// Filter typed by the user.
    pub filter: String,
}

/// Library-browser state for a model in the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerItemState {
    /// "auto" router sentinel at the top of the list.
    Auto,
    /// Deployment exists and is healthy — can be selected as the session endpoint.
    Loaded,
    /// Present in fleet_model_library on one or more nodes, but not deployed.
    OnDisk,
    /// Only in fleet_model_catalog — not on any node yet.
    Catalog,
    /// In-flight download job (queued or running).
    Downloading,
}

#[derive(Debug, Clone)]
pub struct ModelPickerItem {
    pub name: String,
    pub tier: i32,
    /// Nodes that host this model (sorted, deduplicated).
    pub nodes: Vec<String>,
    /// Resolved endpoint URL for the first available node (used on select).
    /// Only meaningful when `state == Loaded`.
    pub endpoint: String,
    /// Is at least one host node online?
    pub online: bool,
    /// Library-browser state — drives icon, colour, and whether Enter can select it.
    pub state: PickerItemState,
    /// Only `Some` for `Loaded`: "host:port" for display.
    pub endpoint_display: Option<String>,
    /// Only `Some` for `Downloading`: 0.0–100.0 percent complete.
    pub progress_pct: Option<f32>,
    /// Pre-rendered right-hand detail string ("on marcus, sophie" / "not yet on fleet" / size / "42%").
    pub detail: String,
    /// Optional runtime tag ("llama.cpp", "mlx", …) displayed after the detail.
    pub runtime: Option<String>,
}

impl ModelPicker {
    /// Returns indices into `items` that match the current filter (case-insensitive substring on name).
    pub fn visible_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.items.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.items.iter().enumerate()
            .filter(|(_, m)| m.name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }
}

/// A single session tab — each has its own conversation, input, and agent.
pub struct SessionTab {
    pub name: String,
    pub session: Option<AgentSession>,
    pub session_id: String,
    pub messages: Vec<DisplayMessage>,
    pub input: InputState,
    pub is_running: bool,
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub status: String,
    pub current_model: String,
    pub tokens_used: usize,
    pub tokens_total: usize,
    pub turn: u32,
    pub tracker: ConversationTracker,
    /// Message queued while agent is running — sent automatically when agent finishes.
    pub queued_message: Option<String>,
}

impl SessionTab {
    /// Signal the currently-running agent to stop. Used by `/clear`, `/cancel`,
    /// `/stop`, and Esc. Returns true if an agent was actually running and
    /// got signalled. Idempotent — safe to call when nothing is running.
    pub fn cancel_current_agent(&mut self) -> bool {
        if !self.is_running {
            return false;
        }
        if let Some(session) = &self.session {
            session.cancel_token.cancel();
        }
        self.is_running = false;
        self.status = "Cancelling…".into();
        true
    }
}

impl SessionTab {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            session: None,
            session_id: String::new(),
            messages: Vec::new(),
            input: InputState::new(),
            is_running: false,
            scroll_offset: 0,
            auto_scroll: true,
            status: "Ready".into(),
            current_model: "auto".into(),
            tokens_used: 0,
            tokens_total: 32_768,
            turn: 0,
            tracker: ConversationTracker::new(),
            queued_message: None,
        }
    }

    /// Push current topic onto Focus Stack (conversation drifted).
    pub fn push_focus(&mut self, title: &str, context: &str, reason: PushReason) {
        self.tracker.focus_stack.push(title.to_string(), context.to_string(), reason);
    }

    /// Pop from Focus Stack (resume previous topic).
    pub fn pop_focus(&mut self) -> Option<String> {
        self.tracker.focus_stack.pop().map(|item| item.title)
    }

    /// Add to backlog.
    pub fn add_backlog(&mut self, title: &str, description: &str, priority: BacklogPriority) {
        self.tracker.backlog.add(title.to_string(), description.to_string(), priority);
    }
}

/// A fleet node with its ForgeFleet daemon and model status.
#[derive(Debug, Clone)]
pub struct FleetNode {
    pub name: String,
    pub ip: String,
    /// OS family as stored in computers.os_family / fleet_nodes.os:
    /// "macos", "linux-ubuntu", "linux-dgx", "windows". Rendered in the
    /// fleet panel header as a pretty name in parens.
    pub os: String,
    /// Is the ForgeFleet daemon running on this node?
    pub daemon_online: bool,
    /// Models loaded on this node.
    pub models: Vec<NodeModel>,
}

/// A model running on a fleet node.
#[derive(Debug, Clone)]
pub struct NodeModel {
    pub name: String,
    /// Runtime engine serving this model: "mlx", "llama.cpp", "vllm",
    /// "mlx_lm", "ollama", or "unknown". Shown as the prefix in the
    /// fleet panel line: `{runtime}:{port}: {short_name}`.
    pub runtime: String,
    pub port: u16,
    pub online: bool,
    pub context_window: usize,
    pub tokens_used: usize,
}

/// Current project info.
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

/// Saved session for switching.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub project: Option<String>,
    pub message_count: usize,
    pub last_active: String,
}

impl App {
    pub async fn new(config: AgentSessionConfig) -> Self {
        let working_dir = config.working_dir.clone();
        let current_project = detect_project(&working_dir);

        let first_tab = SessionTab::new("Session 1");

        Self {
            config,
            commands: CommandRegistry::new(),
            tabs: vec![first_tab],
            active_tab: 0,
            frame: 0,
            should_quit: false,
            fleet_nodes: fleet_nodes_from_db().await,
            current_project,
            working_dir,
            brain_status: None,
            picker: None,
        }
    }

    /// Get the active tab.
    pub fn tab(&self) -> &SessionTab {
        &self.tabs[self.active_tab]
    }

    /// Get the active tab mutably.
    pub fn tab_mut(&mut self) -> &mut SessionTab {
        &mut self.tabs[self.active_tab]
    }

    /// Create a new tab and switch to it.
    pub fn new_tab(&mut self) {
        let name = format!("Session {}", self.tabs.len() + 1);
        self.tabs.push(SessionTab::new(&name));
        self.active_tab = self.tabs.len() - 1;
    }

    /// Switch to next tab.
    pub fn next_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
    }

    /// Switch to previous tab.
    pub fn prev_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.active_tab = if self.active_tab == 0 { self.tabs.len() - 1 } else { self.active_tab - 1 };
        }
    }

    /// Close current tab (unless it's the last one).
    pub fn close_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.tabs.remove(self.active_tab);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            }
        }
    }

    /// Process an agent event and update active tab.
    pub fn handle_event(&mut self, event: AgentEvent) {
        let tab = &mut self.tabs[self.active_tab];

        if let Some(display) = crate::messages::event_to_display(&event) {
            tab.messages.push(display);
        }

        match &event {
            AgentEvent::TurnComplete { turn, .. } => { tab.turn = *turn; }
            AgentEvent::TokenWarning { usage_pct, estimated_tokens, .. } => {
                tab.tokens_used = *estimated_tokens;
                tab.status = format!("Context: {usage_pct:.0}%");
            }
            AgentEvent::Done { .. } => { tab.is_running = false; tab.status = "Ready".into(); }
            AgentEvent::Error { message, .. } => {
                tab.is_running = false;
                tab.status = format!("Error: {}", &message[..message.len().min(50)]);
            }
            AgentEvent::Status { message, .. } => { tab.status = message.clone(); }
            _ => {}
        }
    }

    /// Submit user input from active tab.
    pub fn submit_input(&mut self) {
        let tab = &mut self.tabs[self.active_tab];
        let text = tab.input.submit();
        if text.is_empty() { return; }
        tab.messages.push(render_user_message(&text));
        tab.is_running = true;
        tab.status = "Thinking...".into();
    }

    /// Get spinner for animation.
    pub fn spinner(&self) -> &'static str {
        const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[(self.frame as usize / 2) % FRAMES.len()]
    }

    /// Get web UI URL.
    pub fn web_url(&self) -> String {
        format!("http://localhost:{}", PORT_WEB)
    }

    /// Tab count.
    pub fn tab_count(&self) -> usize { self.tabs.len() }
}

/// Detect project from working directory (check for FORGEFLEET.md, Cargo.toml, package.json).
fn detect_project(dir: &std::path::Path) -> Option<ProjectInfo> {
    // Check for FORGEFLEET.md
    let ff_md = dir.join("FORGEFLEET.md");
    if ff_md.exists() {
        let name = dir.file_name()?.to_str()?.to_string();
        return Some(ProjectInfo {
            id: name.clone(),
            name,
            path: dir.to_path_buf(),
        });
    }

    // Check for Cargo.toml with package name
    let cargo = dir.join("Cargo.toml");
    if cargo.exists() {
        let name = dir.file_name()?.to_str()?.to_string();
        return Some(ProjectInfo {
            id: name.clone(),
            name,
            path: dir.to_path_buf(),
        });
    }

    // Check for package.json
    let pkg = dir.join("package.json");
    if pkg.exists() {
        let name = dir.file_name()?.to_str()?.to_string();
        return Some(ProjectInfo {
            id: name.clone(),
            name,
            path: dir.to_path_buf(),
        });
    }

    None
}

/// Load the fleet topology from Postgres. Returns an empty vec if the
/// database is unreachable — the TUI health-check loop will populate it later.
async fn fleet_nodes_from_db() -> Vec<FleetNode> {
    // Read fleet.toml to get the database URL.
    let Some(home) = dirs::home_dir() else { return Vec::new(); };
    let config_path = home.join(".forgefleet/fleet.toml");
    let Ok(toml_str) = std::fs::read_to_string(&config_path) else { return Vec::new(); };
    let Ok(config) = toml::from_str::<ff_core::config::FleetConfig>(&toml_str) else { return Vec::new(); };
    let db_url = config.database.url.trim().to_string();
    if db_url.is_empty() { return Vec::new(); }

    let pool = match sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&db_url)
        .await
    {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let nodes = match ff_db::pg_list_nodes(&pool).await {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    // Prefer new lifecycle `fleet_model_deployments` (what's actually running); fall back
    // to legacy `fleet_models` (configured/desired models) when no deployments exist yet.
    let deployments = ff_db::pg_list_deployments(&pool, None).await.unwrap_or_default();
    let legacy_models = ff_db::pg_list_models(&pool).await.unwrap_or_default();

    // Extract (runtime, model_id) from a raw "{runtime}:{model}" string.
    // Legacy `fleet_models.name` was stored with this prefix; new V14
    // `computer_model_deployments` has a separate runtime column, but we
    // still keep this parser for the fallback path.
    fn split_runtime(raw: &str) -> (Option<String>, String) {
        const KNOWN_RUNTIMES: &[&str] = &[
            "mlx", "mlx_lm", "MLX", "llama.cpp", "LLAMA.CPP", "vllm", "VLLM",
            "ollama", "unknown",
        ];
        let raw = raw.trim();
        for rt in KNOWN_RUNTIMES {
            let p = format!("{}:", rt);
            if let Some(rest) = raw.strip_prefix(&p) {
                return (Some(rt.to_string()), rest.trim().to_string());
            }
        }
        // Old placeholder like "deploy:55000" — no runtime, treat name as-is.
        if let Some(rest) = raw.strip_prefix("deploy:") {
            return (None, rest.trim().to_string());
        }
        (None, raw.to_string())
    }

    // Infer runtime from the model filename/id itself. The file's naming
    // conventions are strong signals:
    //   .gguf, -Q4_K_M, -Q5_K_M, -Q8_0, -Q6_K → llama.cpp
    //   -mlx, -4bit-mlx, -mlx-, `-4bit` without .gguf → mlx
    //   .safetensors with -FP8 / -BF16 → vllm
    //   ollama-style short names (no dash) → ollama
    // Returns None when nothing conclusive.
    fn infer_runtime_from_name(name: &str) -> Option<String> {
        let lower = name.to_lowercase();
        // Explicit MLX markers first (strongest signal)
        if lower.ends_with("-mlx")
            || lower.contains("-mlx-")
            || lower.contains("-4bit-mlx")
            || lower.contains("-mlx_lm")
        {
            return Some("mlx".to_string());
        }
        // llama.cpp GGUF markers
        if lower.ends_with(".gguf")
            || lower.contains("-q4_k_m")
            || lower.contains("-q4_k_s")
            || lower.contains("-q5_k_m")
            || lower.contains("-q5_k_s")
            || lower.contains("-q6_k")
            || lower.contains("-q8_0")
            || lower.contains("-q3_k")
            || lower.contains("-q2_k")
            || lower.contains("-iq4")
            || lower.contains("-ud-q")
        {
            return Some("llama.cpp".to_string());
        }
        // MLX also uses `-4bit` / `-8bit` quantization tags in its
        // mlx-community naming convention. If no .gguf suffix appeared
        // above, this is probably MLX.
        if lower.contains("-4bit") || lower.contains("-8bit") || lower.contains("4bit-") {
            return Some("mlx".to_string());
        }
        // vLLM / HF safetensors
        if lower.ends_with(".safetensors")
            || lower.contains("-fp8")
            || lower.contains("-bf16")
            || lower.contains("-fp16")
        {
            return Some("vllm".to_string());
        }
        None
    }

    // Shorten model file names for display. Strips common quantization +
    // file-format suffixes so "Qwen3-Coder-30B-A3B-Instruct-Q4_K_M.gguf"
    // shows as "Qwen3-Coder-30B-A3B-Instruct". Caps at 40 chars to keep
    // the fleet panel line readable.
    fn short_model_name(raw: &str) -> String {
        let mut s = raw.trim().to_string();
        // Drop file extensions first.
        for ext in [".gguf", ".safetensors", ".bin", ".ggml"] {
            if let Some(rest) = s.strip_suffix(ext) {
                s = rest.to_string();
            }
        }
        // Drop trailing quantization / precision markers. Case-insensitive.
        let quants = [
            "-Q2_K", "-Q3_K_S", "-Q3_K_M", "-Q3_K_L",
            "-Q4_0", "-Q4_K_S", "-Q4_K_M", "-Q5_0", "-Q5_K_S", "-Q5_K_M",
            "-Q6_K", "-Q8_0",
            "-F16", "-FP16", "-FP8", "-BF16",
            "-UD-Q4_K_M", "-UD",
            "-4bit", "-8bit",
        ];
        loop {
            let lower = s.to_lowercase();
            let mut changed = false;
            for q in &quants {
                let qlow = q.to_lowercase();
                if lower.ends_with(&qlow) {
                    s.truncate(s.len() - q.len());
                    changed = true;
                    break;
                }
            }
            if !changed { break; }
        }
        // Cap at 40 chars.
        if s.chars().count() > 40 {
            s = s.chars().take(37).collect::<String>() + "…";
        }
        // Defensive: if we have nothing meaningful (e.g. all digits like
        // "55000"), fall back to a marker.
        if s.is_empty() || s.chars().all(|c| c.is_ascii_digit()) {
            return format!("port-{}", s);
        }
        s
    }

    nodes
        .into_iter()
        .map(|n| {
            // Deployments on this node.
            let mut node_models: Vec<NodeModel> = deployments
                .iter()
                .filter(|d| d.node_name == n.name)
                .map(|d| {
                    // Runtime resolution, most-specific first:
                    //   1. Prefix encoded in catalog_id ("mlx:gemma-4")
                    //   2. `computer_model_deployments.runtime` column
                    //   3. The fleet node's default runtime (from
                    //      `fleet_members.runtime` — set per-host at
                    //      enrollment, follows reference_runtime_choice_policy.md:
                    //      Mac→mlx, Linux→llama.cpp, DGX→vllm, Windows→
                    //      llama.cpp/ollama).
                    //   4. "unknown" (last resort)
                    let (parsed_rt, raw_name) = match d.catalog_id.as_deref() {
                        Some(cid) => split_runtime(cid),
                        None => (None, String::new()),  // empty name triggers fallback below
                    };
                    // Runtime resolution (most-specific first):
                    //   1. Prefix in catalog_id ("mlx:gemma-4")
                    //   2. Inferred from model filename (-mlx, .gguf, etc.)
                    //   3. deployments.runtime column
                    //   4. fleet_members.runtime (host default)
                    //   5. "unknown"
                    let runtime = parsed_rt
                        .filter(|r| r != "unknown")
                        .or_else(|| infer_runtime_from_name(&raw_name))
                        .or_else(|| {
                            let r = d.runtime.trim();
                            if r.is_empty() || r == "unknown" { None }
                            else { Some(r.to_string()) }
                        })
                        .unwrap_or_else(|| {
                            if n.runtime.trim().is_empty() || n.runtime == "unknown" {
                                "unknown".to_string()
                            } else {
                                n.runtime.clone()
                            }
                        });
                    // Model-name resolution:
                    //   1. Non-empty raw_name from catalog_id → shorten
                    //   2. Otherwise "(unknown model)" (better than "port-X"
                    //      which obscured that we don't know the model)
                    let name = if raw_name.trim().is_empty() {
                        "(unknown model)".to_string()
                    } else {
                        short_model_name(&raw_name)
                    };
                    NodeModel {
                        name,
                        runtime,
                        port: d.port as u16,
                        online: d.health_status == "healthy",
                        context_window: d.context_window.unwrap_or(32_768) as usize,
                        tokens_used: d.tokens_used as usize,
                    }
                })
                .collect();
            // If nothing deployed, show legacy fleet_models entries (existing pattern pre-V11).
            if node_models.is_empty() {
                node_models = legacy_models
                    .iter()
                    .filter(|m| m.node_name == n.name)
                    .map(|m| {
                        let (parsed_rt, raw_name) = split_runtime(&m.name);
                        // Same runtime-fallback as above — prefer the
                        // parsed prefix, then the fleet node's declared
                        // runtime, then "unknown".
                        // Same runtime cascade as the deployments path:
                        // parsed prefix → filename inference → host default.
                        let runtime = parsed_rt
                            .filter(|r| r != "unknown")
                            .or_else(|| infer_runtime_from_name(&raw_name))
                            .unwrap_or_else(|| {
                                if n.runtime.trim().is_empty() || n.runtime == "unknown" {
                                    "unknown".to_string()
                                } else {
                                    n.runtime.clone()
                                }
                            });
                        let name = if raw_name.trim().is_empty() {
                            "(unknown model)".to_string()
                        } else {
                            short_model_name(&raw_name)
                        };
                        NodeModel {
                            name,
                            runtime,
                            port: m.port as u16,
                            online: false,
                            context_window: 32_768,
                            tokens_used: 0,
                        }
                    })
                    .collect();
            }
            FleetNode {
                name: n.name,
                ip: n.ip,
                os: n.os,
                daemon_online: false,
                models: node_models,
            }
        })
        .collect()
}
