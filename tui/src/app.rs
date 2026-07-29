//! Application state and the main event loop.

use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ff_agent::agent_loop::{AgentEvent, AgentSession};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::backend::Backend;
use crate::slash::{self, Ctrl};
use crate::ui;

/// One entry in the transcript.
pub enum Item {
    User(String),
    Assistant(String),
    Tool {
        name: String,
        summary: String,
        done: Option<(bool, u64)>, // (is_error, duration_ms)
    },
    Note(String),
    Error(String),
}

pub struct App {
    pub backend: Backend,
    pub working_dir: PathBuf,
    pub items: Vec<Item>,
    pub input: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    /// Lines scrolled up from the bottom (0 = pinned to bottom).
    pub scroll_up: u16,
    pub running: bool,
    pub spinner_tick: usize,
    pub status: String,
    pub turn: u32,
    pub ctx_pct: Option<f64>,
    pub quit: bool,
    session: Arc<Mutex<Option<AgentSession>>>,
    event_rx: mpsc::Receiver<AgentEvent>,
    event_tx: mpsc::Sender<AgentEvent>,
    ctrl_rx: mpsc::Receiver<Ctrl>,
    ctrl_tx: mpsc::Sender<Ctrl>,
    abort: Option<JoinHandle<()>>,
    /// Rolling plain-text transcript for cloud-CLI context (single-shot CLIs
    /// are stateless — we resend a capped conversation each prompt).
    cloud_log: String,
}

/// Restore the terminal on drop, whatever happens.
struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

impl App {
    pub fn new(backend: Backend, working_dir: PathBuf) -> Self {
        let session = backend
            .agent_session(&working_dir)
            .map(|s| Arc::new(Mutex::new(Some(s))))
            .unwrap_or_else(|| Arc::new(Mutex::new(None)));
        let (event_tx, event_rx) = mpsc::channel(256);
        let (ctrl_tx, ctrl_rx) = mpsc::channel(64);
        Self {
            status: backend.label(),
            backend,
            working_dir,
            items: Vec::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            scroll_up: 0,
            running: false,
            spinner_tick: 0,
            turn: 0,
            ctx_pct: None,
            quit: false,
            session,
            event_rx,
            event_tx,
            ctrl_rx,
            ctrl_tx,
            abort: None,
            cloud_log: String::new(),
        }
    }

    /// Rebuild the agent session after a backend/model change.
    pub fn reset_session(&mut self) {
        self.session = self
            .backend
            .agent_session(&self.working_dir)
            .map(|s| Arc::new(Mutex::new(Some(s))))
            .unwrap_or_else(|| Arc::new(Mutex::new(None)));
        self.cloud_log.clear();
        self.turn = 0;
        self.ctx_pct = None;
    }

    pub fn push_note(&mut self, text: impl Into<String>) {
        self.items.push(Item::Note(text.into()));
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.items.push(Item::Error(text.into()));
    }

    pub fn set_backend(&mut self, backend: Backend) {
        self.status = backend.label();
        self.backend = backend;
        self.reset_session();
    }

    fn submit(&mut self) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() || self.running {
            if self.running {
                self.push_note("agent is running — Esc to abort");
            }
            return;
        }
        self.history.push(prompt.clone());
        self.history_idx = None;
        self.input.clear();
        self.cursor = 0;

        if let Some(rest) = prompt.strip_prefix('/') {
            let tx = self.ctrl_tx.clone();
            slash::dispatch(self, rest, &tx);
            return;
        }

        self.items.push(Item::User(prompt.clone()));
        self.running = true;
        self.scroll_up = 0;

        if self.backend.is_agent() {
            self.dispatch_agent(prompt);
        } else {
            self.dispatch_cloud(prompt);
        }
    }

    fn dispatch_agent(&mut self, prompt: String) {
        let session = self.session.clone();
        let tx = self.event_tx.clone();
        let handle = tokio::spawn(async move {
            let mut guard = session.lock().await;
            match guard.as_mut() {
                Some(s) => {
                    let _ = s.run(&prompt, Some(tx.clone())).await;
                }
                None => {
                    let _ = tx
                        .send(AgentEvent::Error {
                            session_id: String::new(),
                            message: "no agent session for this backend".into(),
                        })
                        .await;
                }
            }
        });
        self.abort = Some(handle);
    }

    fn dispatch_cloud(&mut self, prompt: String) {
        let Backend::CloudCli { cli } = &self.backend else {
            return;
        };
        let cli = cli.clone();
        let cwd = self.working_dir.clone();
        // Resend a capped rolling transcript so the stateless CLI has context.
        self.cloud_log.push_str(&format!("\n\nUser: {prompt}"));
        let mut context = self.cloud_log.clone();
        const CAP: usize = 24_000;
        if context.len() > CAP {
            let cut = context.len() - CAP;
            context = format!("…(earlier turns trimmed)…{}", &context[cut..]);
        }
        let tx = self.event_tx.clone();
        let handle = tokio::spawn(async move {
            let res = ff_agent::cli_executor::execute_cli_in_dir(
                &cli,
                &context,
                &[],
                Some(&cwd),
                Some(Duration::from_secs(600)),
            )
            .await;
            match res {
                Ok(r) if r.exit_code == 0 && !r.stdout.trim().is_empty() => {
                    let _ = tx
                        .send(AgentEvent::AssistantText {
                            session_id: String::new(),
                            text: r.stdout,
                        })
                        .await;
                    let _ = tx
                        .send(AgentEvent::Done {
                            session_id: String::new(),
                            final_text: String::new(),
                        })
                        .await;
                }
                Ok(r) => {
                    let _ = tx
                        .send(AgentEvent::Error {
                            session_id: String::new(),
                            message: format!(
                                "{cli} exited {}: {}",
                                r.exit_code,
                                r.stderr.chars().take(400).collect::<String>()
                            ),
                        })
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(AgentEvent::Error {
                            session_id: String::new(),
                            message: format!("{cli} failed: {e:#}"),
                        })
                        .await;
                }
            }
        });
        self.abort = Some(handle);
    }

    fn abort_run(&mut self) {
        if let Some(h) = self.abort.take() {
            h.abort();
        }
        self.running = false;
        self.push_note("aborted");
    }

    fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AssistantText { text, .. } => {
                if !text.trim().is_empty() {
                    self.cloud_log.push_str(&format!("\n\nAssistant: {text}"));
                }
                // Per-turn chunk: append to the current assistant item or start one.
                match self.items.last_mut() {
                    Some(Item::Assistant(buf)) => {
                        buf.push_str("\n\n");
                        buf.push_str(&text);
                    }
                    _ => self.items.push(Item::Assistant(text)),
                }
            }
            AgentEvent::ToolStart {
                tool_name,
                input_json,
                ..
            } => {
                let summary = summarize_tool_input(&input_json);
                self.items.push(Item::Tool {
                    name: tool_name,
                    summary,
                    done: None,
                });
            }
            AgentEvent::ToolEnd {
                result,
                is_error,
                duration_ms,
                ..
            } => {
                // Match the most recent unfinished tool item.
                if let Some(item) = self.items.iter_mut().rev().find_map(|i| match i {
                    Item::Tool { done: None, .. } => Some(i),
                    _ => None,
                }) && let Item::Tool { done, .. } = item
                {
                    *done = Some((is_error, duration_ms));
                }
                if is_error {
                    self.items.push(Item::Error(format!(
                        "tool error: {}",
                        result.chars().take(200).collect::<String>()
                    )));
                }
            }
            AgentEvent::TurnComplete { turn, .. } => {
                self.turn = turn;
            }
            AgentEvent::Status { message, .. } | AgentEvent::System { message, .. } => {
                self.push_note(message);
            }
            AgentEvent::Compaction {
                messages_before,
                messages_after,
                ..
            } => {
                self.push_note(format!(
                    "context compacted: {messages_before} → {messages_after} messages"
                ));
            }
            AgentEvent::TokenWarning { usage_pct, .. } => {
                self.ctx_pct = Some(usage_pct);
            }
            AgentEvent::Error { message, .. } => {
                self.push_error(message);
                self.running = false;
                self.abort = None;
            }
            AgentEvent::Done { final_text, .. } => {
                if !final_text.trim().is_empty() {
                    match self.items.last_mut() {
                        Some(Item::Assistant(buf)) => {
                            if !buf.contains(final_text.trim()) {
                                buf.push_str(&final_text);
                            }
                        }
                        _ => self.items.push(Item::Assistant(final_text)),
                    }
                }
                self.running = false;
                self.abort = None;
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match (key.code, ctrl, alt) {
            (KeyCode::Char('c'), true, _) => {
                if self.input.is_empty() {
                    self.quit = true;
                } else {
                    self.input.clear();
                    self.cursor = 0;
                }
            }
            (KeyCode::Char('d'), true, _) => self.quit = true,
            (KeyCode::Esc, _, _) => {
                if self.running {
                    self.abort_run();
                } else {
                    self.scroll_up = 0;
                }
            }
            (KeyCode::Enter, false, false) => self.submit(),
            (KeyCode::Enter, _, true) | (KeyCode::Char('j'), true, _) => self.insert('\n'),
            (KeyCode::Backspace, _, _) => self.backspace(),
            (KeyCode::Delete, _, _) => self.delete(),
            (KeyCode::Left, _, _) => self.cursor = self.cursor.saturating_sub(1),
            (KeyCode::Right, _, _) => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count())
            }
            (KeyCode::Home, _, _) => self.cursor = 0,
            (KeyCode::End, _, _) => self.cursor = self.input.chars().count(),
            (KeyCode::Up, _, _) => self.history_move(-1),
            (KeyCode::Down, _, _) => self.history_move(1),
            (KeyCode::PageUp, _, _) => self.scroll_up = self.scroll_up.saturating_add(10),
            (KeyCode::PageDown, _, _) => self.scroll_up = self.scroll_up.saturating_sub(10),
            (KeyCode::Char(c), _, _) => self.insert(c),
            _ => {}
        }
    }

    fn insert(&mut self, c: char) {
        let idx = byte_index(&self.input, self.cursor);
        self.input.insert(idx, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let idx = byte_index(&self.input, self.cursor - 1);
            self.input.remove(idx);
            self.cursor -= 1;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.input.chars().count() {
            let idx = byte_index(&self.input, self.cursor);
            self.input.remove(idx);
        }
    }

    fn history_move(&mut self, dir: i32) {
        if self.history.is_empty() {
            return;
        }
        let len = self.history.len() as i32;
        let idx = match (self.history_idx, dir) {
            (None, -1) => len - 1,
            (Some(i), -1) => (i as i32 - 1).max(0),
            (Some(i), 1) if (i as i32) < len - 1 => i as i32 + 1,
            (Some(_), 1) => {
                self.history_idx = None;
                self.input.clear();
                self.cursor = 0;
                return;
            }
            _ => return,
        };
        self.history_idx = Some(idx as usize);
        self.input = self.history[idx as usize].clone();
        self.cursor = self.input.chars().count();
    }
}

fn byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// One-line summary of a tool call for the transcript: tool(JSON) capped.
fn summarize_tool_input(input_json: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(input_json).unwrap_or_default();
    let brief = match &v {
        serde_json::Value::Object(map) => map
            .iter()
            .take(2)
            .map(|(k, v)| {
                let val = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                format!("{k}={}", val.chars().take(60).collect::<String>())
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    };
    brief.chars().take(100).collect()
}

pub async fn run(backend: Backend, working_dir: PathBuf) -> Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let _guard = TermGuard;
    let backend_term = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend_term)?;

    let mut app = App::new(backend, working_dir);
    app.push_note("ff tui — /help for commands, /backend to switch LLMs");

    while !app.quit {
        // Drain control messages (backend switches, mode changes).
        while let Ok(ctrl) = app.ctrl_rx.try_recv() {
            match ctrl {
                Ctrl::SetBackend(b) => app.set_backend(b),
                Ctrl::SetPermission(mode) => {
                    if let Some(s) = app.session.lock().await.as_mut() {
                        s.config.permission_mode = mode;
                    }
                }
                Ctrl::SetOutputStyle(style) => {
                    if let Some(s) = app.session.lock().await.as_mut() {
                        s.config.output_style = style;
                    }
                }
                Ctrl::Note(n) => app.push_note(n),
                Ctrl::Error(e) => app.push_error(e),
            }
        }
        // Drain agent events first so the UI reflects them this frame.
        while let Ok(ev) = app.event_rx.try_recv() {
            app.on_agent_event(ev);
        }
        app.spinner_tick = app.spinner_tick.wrapping_add(1);

        terminal.draw(|f| ui::render(f, &app))?;

        if event::poll(Duration::from_millis(60))?
            && let Event::Key(key) = event::read()?
        {
            app.on_key(key);
        }
    }
    Ok(())
}

/// Field accessors for slash commands (kept small and explicit).
impl App {
    pub fn model_mut_endpoint(&mut self, model: String) {
        match &mut self.backend {
            Backend::Endpoint { model: m, .. } | Backend::Router { model: m, .. } => {
                *m = model;
                self.status = self.backend.label();
                self.reset_session();
            }
            Backend::CloudCli { .. } => {
                self.push_note("cloud CLI backends pick their own model");
            }
        }
    }
}
