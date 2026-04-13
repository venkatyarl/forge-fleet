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

fn render_tab_bar(frame: &mut Frame, area: Rect, app: &App, _theme: &Theme) {
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
    spans.push(Span::styled(" Ctrl+T: new │ Ctrl+N/P: switch │ Ctrl+W: close ", Style::default().fg(Color::Rgb(71, 85, 105))));

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(30, 41, 59))),
        area,
    );
}

fn render_header(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let tab = app.tab();
    let spinner = if tab.is_running { app.spinner() } else { "●" };
    let spinner_color = if tab.is_running { Color::Rgb(251, 191, 36) } else { Color::Rgb(74, 222, 128) };

    // Show project name + working directory path
    let working_dir = app.config.working_dir.to_string_lossy();
    let project_display = app.current_project.as_ref()
        .map(|p| format!(" │ {} ({}) ", p.name, working_dir))
        .unwrap_or_else(|| format!(" │ {} ", working_dir));

    let header = Line::from(vec![
        Span::styled(format!(" {spinner} "), Style::default().fg(spinner_color)),
        Span::styled("ForgeFleet", theme.header),
        Span::styled(&project_display, Style::default().fg(Color::Rgb(139, 92, 246))),
        Span::styled(
            format!("│ {} │ Turn: {}/{} │ ",
                &tab.current_model[..tab.current_model.len().min(30)],
                tab.turn, app.config.max_turns,
            ),
            theme.status_text,
        ),
        Span::styled(app.web_url(), Style::default().fg(Color::Rgb(99, 102, 241))),
    ]);

    frame.render_widget(Paragraph::new(header).style(Style::default().bg(Color::Rgb(30, 41, 59))), area);
}

fn render_body(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    // Three columns: left sidebar (Focus Stack + Backlog), center (Chat), right sidebar (Fleet)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(18),  // left: Focus Stack + Backlog
            Constraint::Percentage(57),  // center: Chat
            Constraint::Percentage(25),  // right: Fleet
        ])
        .split(area);

    render_left_sidebar(frame, body_chunks[0], app, theme);
    render_messages(frame, body_chunks[1], app, theme);
    render_right_sidebar(frame, body_chunks[2], app, theme);
}

fn render_left_sidebar(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" Context ", Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let tab = app.tab();
    let mut lines = Vec::new();

    // Project
    if let Some(project) = &app.current_project {
        lines.push(Line::from(vec![
            Span::styled(" 📁 ", Style::default()),
            Span::styled(&project.name, Style::default().fg(Color::Rgb(139, 92, 246)).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(""));
    }

    // Brain status — Hive Mind / Fleet Brain / Project Memory
    if let Some(status) = &app.brain_status {
        lines.push(Line::from(vec![
            Span::styled(" Memory ", Style::default().fg(Color::Rgb(167, 243, 208)).add_modifier(Modifier::BOLD)),
        ]));
        let p_color = if status.project_entries > 0 { Color::Rgb(74, 222, 128) } else { Color::Rgb(71, 85, 105) };
        let b_color = if status.brain_entries > 0 { Color::Rgb(74, 222, 128) } else { Color::Rgb(71, 85, 105) };
        let h_color = if status.hive_entries > 0 { Color::Rgb(74, 222, 128) } else { Color::Rgb(71, 85, 105) };
        lines.push(Line::from(vec![
            Span::styled("  P:", Style::default().fg(Color::Rgb(100, 116, 139))),
            Span::styled(format!("{}", status.project_entries), Style::default().fg(p_color)),
            Span::styled(" B:", Style::default().fg(Color::Rgb(100, 116, 139))),
            Span::styled(format!("{}", status.brain_entries), Style::default().fg(b_color)),
            Span::styled(" H:", Style::default().fg(Color::Rgb(100, 116, 139))),
            Span::styled(format!("{}", status.hive_entries), Style::default().fg(h_color)),
        ]));
        lines.push(Line::from(""));
    }

    // Focus Stack (FILO) — real data from session tracker
    let stack_depth = tab.tracker.focus_stack.depth();
    lines.push(Line::from(vec![
        Span::styled(" Focus Stack ", Style::default().fg(Color::Rgb(251, 191, 36)).add_modifier(Modifier::BOLD)),
        Span::styled(format!("({})", stack_depth), Style::default().fg(Color::Rgb(100, 116, 139))),
    ]));

    if stack_depth == 0 {
        lines.push(Line::from(Span::styled("  (empty)", Style::default().fg(Color::Rgb(71, 85, 105)))));
        lines.push(Line::from(Span::styled("  /push <topic>", Style::default().fg(Color::Rgb(71, 85, 105)))));
    } else {
        {
            let summary = tab.tracker.focus_stack.summary();
            for entry in summary.iter().take(5) {
                let filled = (entry.progress * 5.0) as usize;
                let progress_bar = format!("{}{}", "█".repeat(filled), "░".repeat(5usize.saturating_sub(filled)));
                let title = entry.title.clone();
                let age = entry.age.clone();
                lines.push(Line::from(vec![
                    Span::styled(format!("  {} ", entry.depth + 1), Style::default().fg(Color::Rgb(251, 191, 36))),
                    Span::styled(title, Style::default().fg(Color::Rgb(226, 232, 240))),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("    [{progress_bar}] {age}"),
                    Style::default().fg(Color::Rgb(71, 85, 105)),
                )));
            }
        }
        lines.push(Line::from(Span::styled("  /pop to resume", Style::default().fg(Color::Rgb(71, 85, 105)))));
    }

    lines.push(Line::from(""));

    // Backlog (FIFO) — real data from session tracker
    let backlog_count = tab.tracker.backlog.len();
    lines.push(Line::from(vec![
        Span::styled(" Backlog ", Style::default().fg(Color::Rgb(125, 211, 252)).add_modifier(Modifier::BOLD)),
        Span::styled(format!("({})", backlog_count), Style::default().fg(Color::Rgb(100, 116, 139))),
    ]));

    if backlog_count == 0 {
        lines.push(Line::from(Span::styled("  (empty)", Style::default().fg(Color::Rgb(71, 85, 105)))));
        lines.push(Line::from(Span::styled("  /backlog <item>", Style::default().fg(Color::Rgb(71, 85, 105)))));
    } else {
        for (_i, item) in tab.tracker.backlog.items().iter().enumerate().take(5) {
            let priority_icon = match item.priority {
                ff_agent::focus_stack::BacklogPriority::Urgent => "🔴",
                ff_agent::focus_stack::BacklogPriority::High => "🟠",
                ff_agent::focus_stack::BacklogPriority::Medium => "🟡",
                ff_agent::focus_stack::BacklogPriority::Low => "🟢",
            };
            lines.push(Line::from(Span::styled(
                format!("  {priority_icon} {}", item.title),
                Style::default().fg(Color::Rgb(148, 163, 184)),
            )));
        }
        if backlog_count > 5 {
            lines.push(Line::from(Span::styled(
                format!("  ... +{} more", backlog_count - 5),
                Style::default().fg(Color::Rgb(71, 85, 105)),
            )));
        }
    }

    lines.push(Line::from(""));

    // Session info
    lines.push(Line::from(Span::styled(" Session", Style::default().fg(theme.fg).add_modifier(Modifier::BOLD))));
    let tab = app.tab();
    lines.push(Line::from(Span::styled(format!("  {}", tab.name), Style::default().fg(Color::Rgb(148, 163, 184)))));
    lines.push(Line::from(Span::styled(format!("  Turn {}/{}", tab.turn, 30), Style::default().fg(Color::Rgb(100, 116, 139)))));
    lines.push(Line::from(Span::styled(format!("  Model: {}", tab.current_model), Style::default().fg(Color::Rgb(100, 116, 139)))));

    // (web URL moved to header)

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
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

    let total_lines = all_lines.len();
    let visible_height = inner.height as usize;

    if visible_height == 0 {
        return;
    }

    // Manual slicing with strict bounds — never exceed visible_height
    let start = if app.tab().auto_scroll {
        total_lines.saturating_sub(visible_height)
    } else {
        total_lines
            .saturating_sub(visible_height)
            .saturating_sub(app.tab().scroll_offset as usize)
    };

    let visible: Vec<Line> = all_lines.into_iter()
        .skip(start)
        .take(visible_height) // strict cap — never pass more lines than the area can hold
        .collect();

    let paragraph = Paragraph::new(visible);
    frame.render_widget(paragraph, inner);
}

fn render_right_sidebar(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" Fleet ", Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let bar_max = (inner.width as usize).saturating_sub(6);
    let mut lines = Vec::new();

    for node in &app.fleet_nodes {
        // Computer status: green = daemon running, red = daemon offline
        let (daemon_icon, daemon_color) = if node.daemon_online {
            ("●", Color::Rgb(74, 222, 128))
        } else {
            ("○", Color::Rgb(248, 113, 113))
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {daemon_icon} "), Style::default().fg(daemon_color)),
            Span::styled(&node.name, Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)),
        ]));

        // Each model: its own green/red + mini token bar
        for model in &node.models {
            let (icon, color) = if model.online {
                ("●", Color::Rgb(74, 222, 128))
            } else {
                ("○", Color::Rgb(248, 113, 113))
            };

            lines.push(Line::from(vec![
                Span::styled(format!("   {icon} "), Style::default().fg(color)),
                Span::styled(&model.name, Style::default().fg(Color::Rgb(148, 163, 184))),
            ]));

            // Mini token bar for each model
            if model.context_window > 0 {
                let pct = if model.context_window > 0 { (model.tokens_used as f64 / model.context_window as f64 * 100.0) as u16 } else { 0 };
                let filled = (bar_max as f64 * pct as f64 / 100.0) as usize;
                let empty = bar_max.saturating_sub(filled);
                let bar_color = if pct < 50 { Color::Rgb(74, 222, 128) } else if pct < 80 { Color::Rgb(251, 191, 36) } else { Color::Rgb(248, 113, 113) };
                let bar = format!("   {}{}  {}K {pct}%", "█".repeat(filled), "░".repeat(empty), model.context_window / 1024);
                lines.push(Line::from(Span::styled(bar, Style::default().fg(bar_color))));
            }
        }
        if node.models.is_empty() {
            lines.push(Line::from(Span::styled("   no models", Style::default().fg(Color::Rgb(71, 85, 105)))));
        }
    }

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
            format!("  │  {} │  Messages: {}  │  v{}",
                if app.tab().name.is_empty() { format!("Session {}", app.active_tab + 1) } else { app.tab().name.clone() },
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
