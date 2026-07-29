//! Slash-command parsing and dispatch.

use tokio::sync::mpsc;

use crate::app::App;
use crate::backend::{self, Backend};

/// Control messages produced asynchronously (backend switching needs to build
/// a router; permission/style changes need the session lock).
pub enum Ctrl {
    SetBackend(Backend),
    SetPermission(String),
    SetOutputStyle(String),
    Note(String),
    Error(String),
}

pub fn dispatch(app: &mut App, rest: &str, ctrl: &mpsc::Sender<Ctrl>) {
    let mut parts = rest.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    let arg = parts.next().unwrap_or("").trim().to_string();
    match cmd.as_str() {
        "quit" | "exit" | "q" => app.quit = true,
        "clear" => {
            app.items.clear();
            app.reset_session();
            app.push_note("transcript + session cleared");
        }
        "help" | "h" | "?" => help(app),
        "status" => {
            let ctx = app
                .ctx_pct
                .map(|p| format!("{p:.0}% ctx"))
                .unwrap_or_else(|| "ctx n/a".into());
            app.push_note(format!(
                "backend: {} · turn {} · {} · cwd {}",
                app.backend.label(),
                app.turn,
                ctx,
                app.working_dir.display()
            ));
        }
        "backend" | "b" => {
            if arg.is_empty() {
                app.push_note(
                    "usage: /backend router | local | endpoint <url> [model] | claude | codex | kimi",
                );
                return;
            }
            app.push_note(format!("switching backend: {arg} …"));
            let tx = ctrl.clone();
            tokio::spawn(async move {
                match backend::parse(&arg, "auto").await {
                    Ok(b) => {
                        let label = b.label();
                        let _ = tx.send(Ctrl::SetBackend(b)).await;
                        let _ = tx
                            .send(Ctrl::Note(format!("backend attached: {label}")))
                            .await;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Ctrl::Error(format!("backend switch failed: {e:#}")))
                            .await;
                    }
                }
            });
        }
        "model" | "m" => {
            if arg.is_empty() {
                app.push_note("usage: /model <name>");
            } else {
                app.model_mut_endpoint(arg.clone());
                app.push_note(format!("model: {arg}"));
            }
        }
        "plan" => {
            let tx = ctrl.clone();
            tokio::spawn(async move {
                let _ = tx.send(Ctrl::SetPermission("plan".into())).await;
            });
            app.push_note("plan mode ON — mutating tools blocked");
        }
        "default" | "yolo" => {
            let tx = ctrl.clone();
            tokio::spawn(async move {
                let _ = tx.send(Ctrl::SetPermission("default".into())).await;
            });
            app.push_note("default permission mode");
        }
        "concise" | "verbose" => {
            let tx = ctrl.clone();
            let style = cmd.clone();
            tokio::spawn(async move {
                let _ = tx.send(Ctrl::SetOutputStyle(style)).await;
            });
            app.push_note(format!("output style: {cmd}"));
        }
        other => app.push_error(format!("unknown command /{other} — /help")),
    }
}

fn help(app: &mut App) {
    for line in [
        "ff tui commands:",
        "  /backend router                      fleet LLM router (local-first + failover)",
        "  /backend local                       localhost:55000 endpoint",
        "  /backend endpoint <url> [model]      any OpenAI-compatible server",
        "  /backend claude|codex|kimi           cloud CLI (single-shot)",
        "  /model <name>                        switch model on the current endpoint/router",
        "  /plan | /default                     read-only planning ↔ normal tools",
        "  /concise | /verbose                  output style",
        "  /status                              backend · turn · context",
        "  /clear                               wipe transcript + session",
        "  /quit                                exit",
        "keys: Enter send · Alt+Enter newline · ↑/↓ history · PgUp/PgDn scroll · Esc abort · Ctrl+C clear/quit",
    ] {
        app.push_note(line);
    }
}
