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

    // Main layout: header, tab bar, body, input, footer
    let has_tabs = app.tab_count() > 1;
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if has_tabs {
            vec![Constraint::Length(1), Constraint::Length(1), Constraint::Min(8), Constraint::Length(4), Constraint::Length(1)]
        } else {
            vec![Constraint::Length(1), Constraint::Length(0), Constraint::Min(8), Constraint::Length(4), Constraint::Length(1)]
        })
        .split(area);

    render_header(frame, main_chunks[0], app, &theme);
    if has_tabs { render_tab_bar(frame, main_chunks[1], app, &theme); }
    render_body(frame, main_chunks[2], app, &theme);
    render_input(frame, main_chunks[3], app, &theme);
    render_footer(frame, main_chunks[4], app, &theme);
}

fn render_tab_bar(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let mut spans = vec![Span::styled(" ", Style::default())];
    for (i, tab) in app.tabs.iter().enumerate() {
        let is_active = i == app.active_tab;
        let style = if is_active {
            Style::default().fg(Color::White).bg(Color::Rgb(99, 102, 241))
        } else {
            Style::default().fg(Color::Rgb(148, 163, 184)).bg(Color::Rgb(30, 41, 59))
        };
        let indicator = if tab.is_running { "⚡" } else { "" };
        spans.push(Span::styled(format!(" {}{} ", tab.name, indicator), style));
        spans.push(Span::styled(" ", Style::default()));
    }
    spans.push(Span::styled(" Ctrl+T: new │ Ctrl+←/→: switch │ Ctrl+W: close ", Style::default().fg(Color::Rgb(71, 85, 105))));

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(30, 41, 59))),
        area,
    );
}

fn render_header(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let tab = app.tab();
    let spinner = if tab.is_running { app.spinner() } else { "●" };
    let spinner_color = if tab.is_running { Color::Rgb(251, 191, 36) } else { Color::Rgb(74, 222, 128) };

    let project_name = app.current_project.as_ref()
        .map(|p| format!(" │ {} ", p.name))
        .unwrap_or_default();

    let header = Line::from(vec![
        Span::styled(format!(" {spinner} "), Style::default().fg(spinner_color)),
        Span::styled("ForgeFleet", theme.header),
        Span::styled(&project_name, Style::default().fg(Color::Rgb(139, 92, 246))),
        Span::styled(
            format!("│ Model: {} │ Turn: {}/{} │ {}",
                &tab.current_model[..tab.current_model.len().min(25)],
                tab.turn, app.config.max_turns,
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

    if app.tab().messages.is_empty() {
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
    let all_lines: Vec<Line> = app.tab().messages.iter()
        .flat_map(|m| m.lines.clone())
        .collect();

    let visible_height = inner.height as usize;
    let total_lines = all_lines.len();

    // Auto-scroll: show last N lines
    let start = if app.tab().auto_scroll {
        total_lines.saturating_sub(visible_height)
    } else {
        total_lines.saturating_sub(visible_height + app.tab().scroll_offset as usize)
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

    // Project info
    if let Some(project) = &app.current_project {
        lines.push(Line::from(vec![
            Span::styled(" Project: ", Style::default().fg(Color::Rgb(100, 116, 139))),
            Span::styled(&project.name, Style::default().fg(Color::Rgb(139, 92, 246)).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(""));
    }

    // Fleet nodes with daemon status + models
    for node in &app.fleet_nodes {
        let (daemon_icon, daemon_color) = if node.daemon_online {
            ("●", Color::Rgb(74, 222, 128))
        } else {
            ("○", Color::Rgb(248, 113, 113))
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {daemon_icon} "), Style::default().fg(daemon_color)),
            Span::styled(&node.name, Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ({})", node.ip), Style::default().fg(Color::Rgb(71, 85, 105))),
        ]));

        // Models on this node
        for model in &node.models {
            let (model_icon, model_color) = if model.online {
                ("▸", Color::Rgb(74, 222, 128))
            } else {
                ("▹", Color::Rgb(100, 116, 139))
            };
            let token_pct = if model.context_window > 0 {
                (model.tokens_used as f64 / model.context_window as f64 * 100.0) as u16
            } else { 0 };

            lines.push(Line::from(vec![
                Span::styled(format!("   {model_icon} "), Style::default().fg(model_color)),
                Span::styled(&model.name, Style::default().fg(Color::Rgb(148, 163, 184))),
            ]));
            if model.online && model.context_window > 0 {
                lines.push(Line::from(Span::styled(
                    format!("     {}K ctx | {token_pct}%", model.context_window / 1024),
                    Style::default().fg(Color::Rgb(71, 85, 105)),
                )));
            }
        }
        if node.models.is_empty() {
            lines.push(Line::from(Span::styled("   no models", Style::default().fg(Color::Rgb(71, 85, 105)))));
        }
    }

    // Current model tokens
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" Model: {}", app.tab().current_model),
        Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
    )));

    let pct = if app.tab().tokens_total > 0 {
        (app.tab().tokens_used as f64 / app.tab().tokens_total as f64 * 100.0) as u16
    } else { 0 };
    let bar_width = ((inner.width as f64 - 2.0) * pct as f64 / 100.0) as u16;
    let bar_color = if pct < 50 { Color::Rgb(74, 222, 128) } else if pct < 80 { Color::Rgb(251, 191, 36) } else { Color::Rgb(248, 113, 113) };

    let bar: String = "█".repeat(bar_width as usize) + &"░".repeat((inner.width as usize).saturating_sub(bar_width as usize + 2));
    lines.push(Line::from(Span::styled(format!(" {bar}"), Style::default().fg(bar_color))));
    lines.push(Line::from(Span::styled(
        format!(" {}/{} ({pct}%)", app.tab().tokens_used, app.tab().tokens_total),
        Style::default().fg(Color::Rgb(100, 116, 139)),
    )));

    // Web UI link
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" Web: {}", app.web_url()),
        Style::default().fg(Color::Rgb(99, 102, 241)),
    )));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

fn render_input(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let border_color = if app.tab().is_running { Color::Rgb(251, 191, 36) } else { theme.border_focused };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            if app.tab().is_running { " Running... " } else { " Message (Enter to send, /help for commands) " },
            Style::default().fg(if app.tab().is_running { Color::Rgb(251, 191, 36) } else { theme.fg }),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = if app.tab().input.text.is_empty() && !app.tab().is_running {
        "Type your message here...".to_string()
    } else {
        app.tab().input.text.clone()
    };

    let style = if app.tab().input.text.is_empty() && !app.tab().is_running {
        theme.input_placeholder
    } else {
        theme.input
    };

    let paragraph = Paragraph::new(text).style(style).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);

    // Show cursor
    if !app.tab().is_running {
        let cursor_x = inner.x + app.tab().input.cursor as u16;
        let cursor_y = inner.y;
        if cursor_x < inner.x + inner.width {
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    // Show suggestions
    if !app.tab().input.suggestions.is_empty() {
        let suggestions_area = Rect {
            x: area.x + 1,
            y: area.y.saturating_sub(app.tab().input.suggestions.len() as u16 + 1),
            width: area.width.min(60),
            height: app.tab().input.suggestions.len() as u16 + 2,
        };

        let items: Vec<Line> = app.tab().input.suggestions.iter().enumerate().map(|(i, s)| {
            let style = if app.tab().input.suggestion_index == Some(i) {
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
        Span::styled(&app.tab().status, theme.status_text),
        Span::styled(
            format!("  │  Session: {}  │  Messages: {}  │  v{}",
                if app.tab().session_id.len() > 8 { &app.tab().session_id[..8] } else { &app.tab().session_id },
                app.tab().messages.len(),
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
