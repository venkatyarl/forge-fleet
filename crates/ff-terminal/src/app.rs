//! App state — the central state container for ForgeFleet Terminal.


use ff_agent::agent_loop::{AgentEvent, AgentSession, AgentSessionConfig};
use ff_agent::commands::CommandRegistry;

use crate::input::InputState;
use crate::messages::{DisplayMessage, render_user_message};

/// Main application state.
pub struct App {
    /// Session configuration.
    pub config: AgentSessionConfig,
    /// The agent session (created on first message).
    pub session: Option<AgentSession>,
    /// Command registry for slash commands.
    pub commands: CommandRegistry,
    /// All rendered messages for display.
    pub messages: Vec<DisplayMessage>,
    /// Input editor state.
    pub input: InputState,
    /// Whether the agent is currently processing.
    pub is_running: bool,
    /// Scroll offset for the message pane (lines from bottom).
    pub scroll_offset: u16,
    /// Auto-scroll to bottom on new messages.
    pub auto_scroll: bool,
    /// Current status message (shown in footer).
    pub status: String,
    /// Frame counter for spinner animation.
    pub frame: u64,
    /// Whether the app should quit.
    pub should_quit: bool,
    /// Fleet node status cache.
    pub fleet_status: Vec<FleetNodeStatus>,
    /// Token usage.
    pub tokens_used: usize,
    pub tokens_total: usize,
    /// Current turn.
    pub turn: u32,
    /// Session ID.
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct FleetNodeStatus {
    pub name: String,
    pub model: String,
    pub online: bool,
}

impl App {
    pub fn new(config: AgentSessionConfig) -> Self {
        Self {
            config,
            session: None,
            commands: CommandRegistry::new(),
            messages: Vec::new(),
            input: InputState::new(),
            is_running: false,
            scroll_offset: 0,
            auto_scroll: true,
            status: "Ready".into(),
            frame: 0,
            should_quit: false,
            fleet_status: default_fleet_status(),
            tokens_used: 0,
            tokens_total: 32_768,
            turn: 0,
            session_id: String::new(),
        }
    }

    /// Process an agent event and update display.
    pub fn handle_event(&mut self, event: AgentEvent) {
        if let Some(display) = crate::messages::event_to_display(&event) {
            self.messages.push(display);
        }

        match &event {
            AgentEvent::TurnComplete { turn, .. } => {
                self.turn = *turn;
            }
            AgentEvent::TokenWarning { usage_pct, estimated_tokens, .. } => {
                self.tokens_used = *estimated_tokens;
                self.status = format!("Context: {usage_pct:.0}%");
            }
            AgentEvent::Done { .. } => {
                self.is_running = false;
                self.status = "Ready".into();
            }
            AgentEvent::Error { message, .. } => {
                self.is_running = false;
                self.status = format!("Error: {}", &message[..message.len().min(50)]);
            }
            AgentEvent::Status { message, .. } => {
                self.status = message.clone();
            }
            _ => {}
        }
    }

    /// Submit user input.
    pub fn submit_input(&mut self) {
        let text = self.input.submit();
        if text.is_empty() { return; }

        // Add user message to display
        self.messages.push(render_user_message(&text));
        self.is_running = true;
        self.status = "Thinking...".into();
    }

    /// Get the spinner character for the current frame.
    pub fn spinner(&self) -> &'static str {
        const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[(self.frame as usize / 2) % FRAMES.len()]
    }

    /// Total lines in the message pane.
    pub fn total_message_lines(&self) -> usize {
        self.messages.iter().map(|m| m.lines.len()).sum()
    }
}

fn default_fleet_status() -> Vec<FleetNodeStatus> {
    vec![
        FleetNodeStatus { name: "Taylor".into(), model: "Gemma-4-31B".into(), online: true },
        FleetNodeStatus { name: "Marcus".into(), model: "Qwen2.5-32B".into(), online: true },
        FleetNodeStatus { name: "Sophie".into(), model: "Qwen2.5-32B".into(), online: true },
        FleetNodeStatus { name: "Priya".into(), model: "Qwen2.5-32B".into(), online: true },
        FleetNodeStatus { name: "James".into(), model: "Qwen2.5-72B".into(), online: true },
        FleetNodeStatus { name: "Ace".into(), model: "—".into(), online: false },
    ]
}
