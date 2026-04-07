//! Rendering — layout and draw the ForgeFleet Terminal UI.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::App;
use crate::theme::Theme;

/// Render the full application UI.
pub fn render(frame: &mut Frame, app: &App) {
    let theme = Theme::default();
    let area = frame.area();

    // Main layout: header, body, input, footer
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),    // header
            Constraint::Min(10),     // body (messages + sidebar)
            Constraint::Length(4),    // input
            Constraint::Length(1),    // footer
        ])
        .split(area);

    render_header(frame, main_chunks[0], app, &theme);
    render_body(frame, main_chunks[1], app, &theme);
    render_input(frame, main_chunks[2], app, &theme);
    render_footer(frame, main_chunks[3], app, &theme);
}

fn render_header(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let spinner = if app.is_running { app.spinner() } else { "●" };
    let spinner_color = if app.is_running { Color::Rgb(251, 191, 36) } else { Color::Rgb(74, 222, 128) };

    let header = Line::from(vec![
        Span::styled(format!(" {spinner} "), Style::default().fg(spinner_color)),
        Span::styled("ForgeFleet Terminal", theme.header),
        Span::styled(
            format!("  │  Model: {}  │  Turn: {}/{}  │  {}",
                &app.config.model[..app.config.model.len().min(20)],
                app.turn, app.config.max_turns,
                &app.config.llm_base_url,
            ),
            theme.status_text,
        ),
    ]);

    frame.render_widget(Paragraph::new(header).style(Style::default().bg(Color::Rgb(30, 41, 59))), area);
}

fn render_body(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    // Split body into messages (80%) and sidebar (20%)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(80),
            Constraint::Percentage(20),
        ])
        .split(area);

    render_messages(frame, body_chunks[0], app, theme);
    render_sidebar(frame, body_chunks[1], app, theme);
}

fn render_messages(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" Chat ", Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.messages.is_empty() {
        let welcome = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled("  Welcome to ForgeFleet Terminal", Style::default().fg(Color::Rgb(139, 92, 246)).add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from(Span::styled("  Type a message to start. The agent will use tools", theme.status_text)),
            Line::from(Span::styled("  (Bash, Read, Edit, etc.) to accomplish your task.", theme.status_text)),
            Line::from(""),
            Line::from(Span::styled("  /help for commands  •  /fleet for node status  •  /exit to quit", theme.command_hint)),
        ]);
        frame.render_widget(welcome, inner);
        return;
    }

    // Collect all lines
    let all_lines: Vec<Line> = app.messages.iter()
        .flat_map(|m| m.lines.clone())
        .collect();

    let visible_height = inner.height as usize;
    let total_lines = all_lines.len();

    // Auto-scroll: show last N lines
    let start = if app.auto_scroll {
        total_lines.saturating_sub(visible_height)
    } else {
        total_lines.saturating_sub(visible_height + app.scroll_offset as usize)
    };

    let visible: Vec<Line> = all_lines.into_iter().skip(start).take(visible_height).collect();

    let paragraph = Paragraph::new(visible).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn render_sidebar(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" Fleet ", Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = Vec::new();

    // Fleet nodes
    for node in &app.fleet_status {
        let (icon, color) = if node.online {
            ("●", Color::Rgb(74, 222, 128))
        } else {
            ("○", Color::Rgb(248, 113, 113))
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {icon} "), Style::default().fg(color)),
            Span::styled(&node.name, Style::default().fg(theme.fg)),
        ]));
        lines.push(Line::from(Span::styled(
            format!("   {}", node.model),
            Style::default().fg(Color::Rgb(100, 116, 139)),
        )));
    }

    // Token gauge
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" Tokens", Style::default().fg(theme.fg).add_modifier(Modifier::BOLD))));

    let pct = if app.tokens_total > 0 {
        (app.tokens_used as f64 / app.tokens_total as f64 * 100.0) as u16
    } else { 0 };
    let bar_width = ((inner.width as f64 - 2.0) * pct as f64 / 100.0) as u16;
    let bar_color = if pct < 50 { Color::Rgb(74, 222, 128) } else if pct < 80 { Color::Rgb(251, 191, 36) } else { Color::Rgb(248, 113, 113) };

    let bar: String = "█".repeat(bar_width as usize) + &"░".repeat((inner.width as usize).saturating_sub(bar_width as usize + 2));
    lines.push(Line::from(Span::styled(format!(" {bar}"), Style::default().fg(bar_color))));
    lines.push(Line::from(Span::styled(
        format!(" {}/{} ({pct}%)", app.tokens_used, app.tokens_total),
        Style::default().fg(Color::Rgb(100, 116, 139)),
    )));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

fn render_input(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let border_color = if app.is_running { Color::Rgb(251, 191, 36) } else { theme.border_focused };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            if app.is_running { " Running... " } else { " Message (Enter to send, /help for commands) " },
            Style::default().fg(if app.is_running { Color::Rgb(251, 191, 36) } else { theme.fg }),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = if app.input.text.is_empty() && !app.is_running {
        "Type your message here...".to_string()
    } else {
        app.input.text.clone()
    };

    let style = if app.input.text.is_empty() && !app.is_running {
        theme.input_placeholder
    } else {
        theme.input
    };

    let paragraph = Paragraph::new(text).style(style).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);

    // Show cursor
    if !app.is_running {
        let cursor_x = inner.x + app.input.cursor as u16;
        let cursor_y = inner.y;
        if cursor_x < inner.x + inner.width {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    // Show suggestions
    if !app.input.suggestions.is_empty() {
        let suggestions_area = Rect {
            x: area.x + 1,
            y: area.y.saturating_sub(app.input.suggestions.len() as u16 + 1),
            width: area.width.min(60),
            height: app.input.suggestions.len() as u16 + 2,
        };

        let items: Vec<Line> = app.input.suggestions.iter().enumerate().map(|(i, s)| {
            let style = if app.input.suggestion_index == Some(i) {
                Style::default().fg(Color::White).bg(Color::Rgb(99, 102, 241))
            } else {
                Style::default().fg(Color::Rgb(148, 163, 184))
            };
            Line::from(Span::styled(format!(" {s} "), style))
        }).collect();

        let suggestions_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(99, 102, 241)))
            .title(" Commands ");

        frame.render_widget(Clear, suggestions_area);
        frame.render_widget(
            Paragraph::new(items).block(suggestions_block),
            suggestions_area,
        );
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let footer = Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(&app.status, theme.status_text),
        Span::styled(
            format!("  │  Session: {}  │  Messages: {}  │  v{}",
                if app.session_id.len() > 8 { &app.session_id[..8] } else { &app.session_id },
                app.messages.len(),
                env!("CARGO_PKG_VERSION"),
            ),
            theme.footer,
        ),
    ]);

    frame.render_widget(
        Paragraph::new(footer).style(Style::default().bg(Color::Rgb(30, 41, 59))),
        area,
    );
}
