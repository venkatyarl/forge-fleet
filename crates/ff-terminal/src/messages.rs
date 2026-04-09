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

/// Render an assistant text message with basic markdown support.
pub fn render_assistant_message(text: &str) -> DisplayMessage {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Agent ", Style::default().fg(Color::Rgb(134, 239, 172)).add_modifier(Modifier::BOLD)),
    ]));

    let mut in_code_block = false;
    let mut in_table = false;

    for line in text.lines() {
        // Code blocks
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            if in_code_block {
                lines.push(Line::from(Span::styled(
                    "  ┌─────────────────────────────────────".to_string(),
                    Style::default().fg(Color::Rgb(100, 116, 139)),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "  └─────────────────────────────────────".to_string(),
                    Style::default().fg(Color::Rgb(100, 116, 139)),
                )));
            }
            continue;
        }

        if in_code_block {
            lines.push(Line::from(Span::styled(
                format!("  │ {line}"),
                Style::default().fg(Color::Rgb(196, 181, 253)), // violet-300
            )));
            continue;
        }

        // Table separator rows (|:---|:---|)
        if is_table_separator(line) {
            in_table = true;
            continue; // skip separator rows
        }

        // Table rows
        if line.starts_with('|') && line.ends_with('|') {
            in_table = true;
            let cells: Vec<&str> = line.split('|')
                .filter(|c| !c.is_empty())
                .map(|c| c.trim())
                .collect();

            let mut spans = Vec::new();
            spans.push(Span::styled("  ", Style::default()));
            for (i, cell) in cells.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" │ ", Style::default().fg(Color::Rgb(100, 116, 139))));
                }
                // Apply inline markdown to cell content
                spans.extend(parse_inline_markdown(cell));
            }
            lines.push(Line::from(spans));
            continue;
        }

        if in_table {
            in_table = false;
        }

        // Headers
        if line.starts_with("#### ") {
            lines.push(Line::from(Span::styled(
                format!("  {}", &line[5..]),
                Style::default().fg(Color::Rgb(251, 191, 36)).add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if line.starts_with("### ") {
            lines.push(Line::from(Span::styled(
                format!("  {}", &line[4..]),
                Style::default().fg(Color::Rgb(251, 191, 36)).add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if line.starts_with("## ") {
            lines.push(Line::from(Span::styled(
                line[3..].to_string(),
                Style::default().fg(Color::Rgb(251, 191, 36)).add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if line.starts_with("# ") {
            lines.push(Line::from(Span::styled(
                line[2..].to_string(),
                Style::default().fg(Color::Rgb(251, 191, 36)).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            continue;
        }

        // Numbered lists
        if let Some(rest) = strip_numbered_list(line) {
            let mut spans = vec![
                Span::styled("  ", Style::default()),
            ];
            // Find the number prefix
            let prefix_end = line.find(". ").unwrap_or(0) + 2;
            spans.push(Span::styled(
                line[..prefix_end].to_string(),
                Style::default().fg(Color::Rgb(134, 239, 172)),
            ));
            spans.extend(parse_inline_markdown(rest));
            lines.push(Line::from(spans));
            continue;
        }

        // Bullet points
        if line.starts_with("- ") || line.starts_with("* ") {
            let content = &line[2..];
            let mut spans = vec![
                Span::styled("  • ", Style::default().fg(Color::Rgb(134, 239, 172))),
            ];
            spans.extend(parse_inline_markdown(content));
            lines.push(Line::from(spans));
            continue;
        }

        // Indented bullet points
        if line.starts_with("  - ") || line.starts_with("  * ") {
            let content = &line[4..];
            let mut spans = vec![
                Span::styled("    ◦ ", Style::default().fg(Color::Rgb(134, 239, 172))),
            ];
            spans.extend(parse_inline_markdown(content));
            lines.push(Line::from(spans));
            continue;
        }

        // Regular text with inline markdown
        let spans = parse_inline_markdown(line);
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    DisplayMessage { lines, role: MessageRole::Assistant }
}

/// Check if a line is a markdown table separator (e.g., |:---|:---|)
fn is_table_separator(line: &str) -> bool {
    if !line.starts_with('|') { return false; }
    line.chars().all(|c| c == '|' || c == ':' || c == '-' || c == ' ')
}

/// Strip numbered list prefix (e.g., "1. " → rest)
fn strip_numbered_list(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    // Must start with a digit
    if !chars.next().map_or(false, |c| c.is_ascii_digit()) {
        return None;
    }
    // Consume remaining digits
    let rest = trimmed.trim_start_matches(|c: char| c.is_ascii_digit());
    if rest.starts_with(". ") {
        Some(&rest[2..])
    } else {
        None
    }
}

/// Parse inline markdown: **bold**, *italic*, `code`, [links]
fn parse_inline_markdown(text: &str) -> Vec<Span<'static>> {
    let base_style = Style::default().fg(Color::Rgb(203, 213, 225)); // slate-300
    let bold_style = Style::default().fg(Color::Rgb(241, 245, 249)).add_modifier(Modifier::BOLD); // slate-100 bold
    let italic_style = Style::default().fg(Color::Rgb(203, 213, 225)).add_modifier(Modifier::ITALIC);
    let code_style = Style::default().fg(Color::Rgb(196, 181, 253)).bg(Color::Rgb(30, 30, 40)); // violet on dark

    let mut spans = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        // Find the next markdown token
        let bold_pos = remaining.find("**");
        let code_pos = remaining.find('`');
        let italic_pos = find_single_asterisk(remaining);

        // Pick the earliest token
        let next = [
            bold_pos.map(|p| (p, "**")),
            code_pos.map(|p| (p, "`")),
            italic_pos.map(|p| (p, "*")),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|(pos, _)| *pos);

        match next {
            None => {
                // No more tokens — push rest as plain text
                if !remaining.is_empty() {
                    spans.push(Span::styled(remaining.to_string(), base_style));
                }
                break;
            }
            Some((pos, "**")) => {
                // Push text before the **
                if pos > 0 {
                    spans.push(Span::styled(remaining[..pos].to_string(), base_style));
                }
                let after = &remaining[pos + 2..];
                if let Some(end) = after.find("**") {
                    spans.push(Span::styled(after[..end].to_string(), bold_style));
                    remaining = &after[end + 2..];
                } else {
                    // No closing ** — treat literally
                    spans.push(Span::styled(remaining[pos..].to_string(), base_style));
                    break;
                }
            }
            Some((pos, "`")) => {
                if pos > 0 {
                    spans.push(Span::styled(remaining[..pos].to_string(), base_style));
                }
                let after = &remaining[pos + 1..];
                if let Some(end) = after.find('`') {
                    spans.push(Span::styled(after[..end].to_string(), code_style));
                    remaining = &after[end + 1..];
                } else {
                    spans.push(Span::styled(remaining[pos..].to_string(), base_style));
                    break;
                }
            }
            Some((pos, "*")) => {
                if pos > 0 {
                    spans.push(Span::styled(remaining[..pos].to_string(), base_style));
                }
                let after = &remaining[pos + 1..];
                if let Some(end) = find_single_asterisk_end(after) {
                    spans.push(Span::styled(after[..end].to_string(), italic_style));
                    remaining = &after[end + 1..];
                } else {
                    spans.push(Span::styled(remaining[pos..].to_string(), base_style));
                    break;
                }
            }
            _ => {
                spans.push(Span::styled(remaining.to_string(), base_style));
                break;
            }
        }
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base_style));
    }

    spans
}

/// Find position of a single `*` that isn't part of `**`
fn find_single_asterisk(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'*' {
            let prev_star = i > 0 && bytes[i - 1] == b'*';
            let next_star = i + 1 < bytes.len() && bytes[i + 1] == b'*';
            if !prev_star && !next_star {
                return Some(i);
            }
        }
    }
    None
}

/// Find the closing single `*` (not `**`)
fn find_single_asterisk_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'*' {
            let next_star = i + 1 < bytes.len() && bytes[i + 1] == b'*';
            if !next_star {
                return Some(i);
            }
        }
    }
    None
}

/// Render a tool start event.
pub fn render_tool_start(tool_name: &str, input_json: &str) -> DisplayMessage {
    let preview = if input_json.len() > 60 {
        let mut end = 60;
        while end > 0 && !input_json.is_char_boundary(end) { end -= 1; }
        format!("{}...", &input_json[..end])
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

    let first_line = result.lines().next().unwrap_or("");
    let preview = if first_line.len() > 100 {
        let mut end = 100;
        while end > 0 && !first_line.is_char_boundary(end) { end -= 1; }
        format!("{}...", &first_line[..end])
    } else {
        first_line.to_string()
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
