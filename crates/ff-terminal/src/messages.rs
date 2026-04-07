//! Message rendering — convert agent events into displayable terminal lines.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use ff_agent::agent_loop::AgentEvent;

/// A rendered message for display in the terminal.
#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub lines: Vec<Line<'static>>,
    pub role: MessageRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    ToolStart,
    ToolEnd,
    Status,
    Error,
    System,
}

/// Render a user message.
pub fn render_user_message(text: &str) -> DisplayMessage {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("You ", Style::default().fg(Color::Rgb(125, 211, 252)).add_modifier(Modifier::BOLD)),
    ]));
    for line in text.lines() {
        lines.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(Color::Rgb(186, 230, 253)), // sky-200
        )));
    }
    lines.push(Line::from(""));
    DisplayMessage { lines, role: MessageRole::User }
}

/// Render an assistant text message.
pub fn render_assistant_message(text: &str) -> DisplayMessage {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Agent ", Style::default().fg(Color::Rgb(134, 239, 172)).add_modifier(Modifier::BOLD)),
    ]));

    let mut in_code_block = false;
    for line in text.lines() {
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::Rgb(139, 92, 246)), // violet
            )));
        } else if in_code_block {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(Color::Rgb(196, 181, 253)), // violet-300
            )));
        } else if line.starts_with("- ") || line.starts_with("* ") {
            lines.push(Line::from(vec![
                Span::styled("  • ", Style::default().fg(Color::Rgb(134, 239, 172))),
                Span::styled(line[2..].to_string(), Style::default().fg(Color::Rgb(203, 213, 225))),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::Rgb(203, 213, 225)), // slate-300
            )));
        }
    }
    lines.push(Line::from(""));
    DisplayMessage { lines, role: MessageRole::Assistant }
}

/// Render a tool start event.
pub fn render_tool_start(tool_name: &str, input_json: &str) -> DisplayMessage {
    let preview = if input_json.len() > 60 {
        format!("{}...", &input_json[..60])
    } else {
        input_json.to_string()
    };

    DisplayMessage {
        lines: vec![Line::from(vec![
            Span::styled("  ⚡ ", Style::default().fg(Color::Rgb(251, 191, 36))),
            Span::styled(tool_name.to_string(), Style::default().fg(Color::Rgb(251, 191, 36)).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {preview}"), Style::default().fg(Color::Rgb(100, 116, 139))),
        ])],
        role: MessageRole::ToolStart,
    }
}

/// Render a tool end event.
pub fn render_tool_end(tool_name: &str, result: &str, is_error: bool, duration_ms: u64) -> DisplayMessage {
    let (icon, color) = if is_error {
        ("✗", Color::Rgb(248, 113, 113))
    } else {
        ("✓", Color::Rgb(74, 222, 128))
    };

    let preview = if result.len() > 100 {
        format!("{}...", &result.lines().next().unwrap_or("")[..result.len().min(100)])
    } else {
        result.lines().next().unwrap_or("").to_string()
    };

    DisplayMessage {
        lines: vec![Line::from(vec![
            Span::styled(format!("  {icon} "), Style::default().fg(color)),
            Span::styled(tool_name.to_string(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ({duration_ms}ms) "), Style::default().fg(Color::Rgb(100, 116, 139))),
            Span::styled(preview, Style::default().fg(Color::Rgb(148, 163, 184))),
        ])],
        role: MessageRole::ToolEnd,
    }
}

/// Render a status message.
pub fn render_status(message: &str) -> DisplayMessage {
    DisplayMessage {
        lines: vec![Line::from(Span::styled(
            format!("  {message}"),
            Style::default().fg(Color::Rgb(100, 116, 139)),
        ))],
        role: MessageRole::Status,
    }
}

/// Render an error message.
pub fn render_error(message: &str) -> DisplayMessage {
    DisplayMessage {
        lines: vec![Line::from(vec![
            Span::styled("  ✗ Error: ", Style::default().fg(Color::Rgb(248, 113, 113)).add_modifier(Modifier::BOLD)),
            Span::styled(message.to_string(), Style::default().fg(Color::Rgb(248, 113, 113))),
        ])],
        role: MessageRole::Error,
    }
}

/// Convert an AgentEvent to a DisplayMessage.
pub fn event_to_display(event: &AgentEvent) -> Option<DisplayMessage> {
    match event {
        AgentEvent::AssistantText { text, .. } => Some(render_assistant_message(text)),
        AgentEvent::ToolStart { tool_name, input_json, .. } => Some(render_tool_start(tool_name, input_json)),
        AgentEvent::ToolEnd { tool_name, result, is_error, duration_ms, .. } => {
            Some(render_tool_end(tool_name, result, *is_error, *duration_ms))
        }
        AgentEvent::Status { message, .. } => Some(render_status(message)),
        AgentEvent::Error { message, .. } => Some(render_error(message)),
        _ => None,
    }
}
