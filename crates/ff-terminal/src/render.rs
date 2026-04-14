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

    // Main layout: header, tab bar, body, input, footer.
    // Input grows with content: 1–6 lines of text + 2 for borders → 3-row min, 8-row max.
    let has_tabs = app.tab_count() > 1;
    let lines_in_input = app.tab().input.line_count().clamp(1, 6) as u16;
    let input_height = lines_in_input + 2; // +2 for top+bottom border
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if has_tabs {
            vec![Constraint::Length(1), Constraint::Length(1), Constraint::Min(4), Constraint::Length(input_height), Constraint::Length(1)]
        } else {
            vec![Constraint::Length(1), Constraint::Length(0), Constraint::Min(4), Constraint::Length(input_height), Constraint::Length(1)]
        })
        .split(area);

    render_header(frame, main_chunks[0], app, &theme);
    if has_tabs { render_tab_bar(frame, main_chunks[1], app, &theme); }
    render_body(frame, main_chunks[2], app, &theme);
    render_input(frame, main_chunks[3], app, &theme);
    render_footer(frame, main_chunks[4], app, &theme);

    // Modal overlay — drawn on top of everything else.
    if app.picker.is_some() {
        render_model_picker(frame, app, &theme);
    }
}

fn render_model_picker(frame: &mut Frame, app: &App, _theme: &Theme) {
    let picker = match app.picker.as_ref() { Some(p) => p, None => return };
    let frame_area = frame.area();
    // Centered popup: 70% width, 60% height (with sensible min/max)
    let popup_w = (frame_area.width as f32 * 0.65) as u16;
    let popup_w = popup_w.clamp(40, 100);
    let popup_h = (frame_area.height as f32 * 0.6) as u16;
    let popup_h = popup_h.clamp(10, 28).min(frame_area.height.saturating_sub(2));
    let popup_x = (frame_area.width.saturating_sub(popup_w)) / 2;
    let popup_y = (frame_area.height.saturating_sub(popup_h)) / 2;
    let popup_area = Rect { x: popup_x, y: popup_y, width: popup_w, height: popup_h };

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(94, 234, 212)))
        .title(Span::styled(" Model Picker ", Style::default().fg(Color::Rgb(94, 234, 212)).add_modifier(Modifier::BOLD)));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 3 { return; }

    // Layout inside popup: filter line, list area, footer help.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // filter
            Constraint::Min(1),    // list
            Constraint::Length(1), // help footer
        ])
        .split(inner);

    // Filter row
    let filter_text = if picker.filter.is_empty() {
        Line::from(vec![
            Span::styled(" Type to filter ", Style::default().fg(Color::Rgb(100, 116, 139)).add_modifier(Modifier::ITALIC)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" filter: ", Style::default().fg(Color::Rgb(100, 116, 139))),
            Span::styled(&picker.filter, Style::default().fg(Color::Rgb(226, 232, 240)).add_modifier(Modifier::BOLD)),
        ])
    };
    frame.render_widget(Paragraph::new(filter_text), chunks[0]);

    // List area
    let list_area = chunks[1];
    let visible_idxs = picker.visible_indices();
    let mut lines: Vec<Line> = Vec::new();

    if picker.loading {
        lines.push(Line::from(Span::styled(" Loading models from fleet…", Style::default().fg(Color::Rgb(251, 191, 36)).add_modifier(Modifier::ITALIC))));
    } else if let Some(err) = &picker.error {
        lines.push(Line::from(Span::styled(format!(" Error: {err}"), Style::default().fg(Color::Rgb(248, 113, 113)))));
    } else if visible_idxs.is_empty() {
        let msg = if picker.items.is_empty() { " No models found in fleet" } else { " No matches" };
        lines.push(Line::from(Span::styled(msg, Style::default().fg(Color::Rgb(100, 116, 139)))));
    } else {
        let max_rows = list_area.height as usize;
        // Simple windowing: keep `selected` visible
        let selected = picker.selected.min(visible_idxs.len().saturating_sub(1));
        let start = if selected >= max_rows { selected + 1 - max_rows } else { 0 };
        for (row, &idx) in visible_idxs.iter().enumerate().skip(start).take(max_rows) {
            let m = &picker.items[idx];
            let is_selected = row == selected;
            let status_dot = if m.online { ("●", Color::Rgb(74, 222, 128)) } else { ("○", Color::Rgb(248, 113, 113)) };
            let row_style = if is_selected {
                Style::default().fg(Color::White).bg(Color::Rgb(99, 102, 241))
            } else {
                Style::default().fg(Color::Rgb(226, 232, 240))
            };
            let nodes_str = m.nodes.join(", ");
            let nodes_color = if is_selected { Color::Rgb(199, 210, 254) } else { Color::Rgb(100, 116, 139) };
            let tier_label = if m.name == "auto" { "[AUTO]".to_string() } else { format!("[T{}]", m.tier) };
            let on_label = if m.name == "auto" { "  fleet router" } else { "  on " };
            let nodes_text = if m.name == "auto" { String::new() } else { nodes_str };
            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", status_dot.0), Style::default().fg(status_dot.1).bg(if is_selected { Color::Rgb(99, 102, 241) } else { Color::Reset })),
                Span::styled(format!("{tier_label} "), if is_selected { row_style } else { Style::default().fg(Color::Rgb(251, 191, 36)) }),
                Span::styled(m.name.clone(), if is_selected { row_style.add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Rgb(226, 232, 240)).add_modifier(Modifier::BOLD) }),
                Span::styled(on_label, Style::default().fg(nodes_color).bg(if is_selected { Color::Rgb(99, 102, 241) } else { Color::Reset })),
                Span::styled(nodes_text, Style::default().fg(nodes_color).bg(if is_selected { Color::Rgb(99, 102, 241) } else { Color::Reset })),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines), list_area);

    // Help footer
    let help = Line::from(vec![
        Span::styled(" Enter", Style::default().fg(Color::Rgb(125, 211, 252)).add_modifier(Modifier::BOLD)),
        Span::styled("=select  ", Style::default().fg(Color::Rgb(100, 116, 139))),
        Span::styled("↑↓", Style::default().fg(Color::Rgb(125, 211, 252)).add_modifier(Modifier::BOLD)),
        Span::styled("=move  ", Style::default().fg(Color::Rgb(100, 116, 139))),
        Span::styled("Esc", Style::default().fg(Color::Rgb(125, 211, 252)).add_modifier(Modifier::BOLD)),
        Span::styled("=cancel  ", Style::default().fg(Color::Rgb(100, 116, 139))),
        Span::styled("Type", Style::default().fg(Color::Rgb(125, 211, 252)).add_modifier(Modifier::BOLD)),
        Span::styled(" to filter", Style::default().fg(Color::Rgb(100, 116, 139))),
    ]);
    frame.render_widget(Paragraph::new(help), chunks[2]);
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
    // Two columns: left sidebar (Focus Stack + Backlog + Fleet), center (Chat).
    // Right sidebar removed 2026-04-14 — session/memory moved to footer, chat gains room.
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(22),  // left: Focus Stack + Backlog + Fleet
            Constraint::Percentage(78),  // center: Chat
        ])
        .split(area);

    render_left_sidebar(frame, body_chunks[0], app, theme);
    render_messages(frame, body_chunks[1], app, theme);
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

    // NOTE: Memory + Session moved to the footer (2026-04-14).
    // Project/Focus Stack/Backlog/Fleet stay in the left sidebar.

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

    // Fleet — node + model status (moved from former right sidebar)
    lines.push(Line::from(Span::styled(" Fleet ", Style::default().fg(theme.fg).add_modifier(Modifier::BOLD))));
    if app.fleet_nodes.is_empty() {
        lines.push(Line::from(Span::styled("  (no nodes)", Style::default().fg(Color::Rgb(71, 85, 105)))));
    } else {
        for node in &app.fleet_nodes {
            let (daemon_icon, daemon_color) = if node.daemon_online {
                ("●", Color::Rgb(74, 222, 128))
            } else {
                ("○", Color::Rgb(248, 113, 113))
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {daemon_icon} "), Style::default().fg(daemon_color)),
                Span::styled(&node.name, Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)),
            ]));
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
            }
        }
    }

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

    let paragraph = Paragraph::new(visible).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

fn render_input(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let border_color = if app.tab().is_running { Color::Rgb(251, 191, 36) } else { theme.border_focused };

    let line_count = app.tab().input.line_count();
    let title = if app.tab().is_running {
        " Running... ".to_string()
    } else if line_count > 6 {
        format!(" Message ({line_count} lines • Enter=send, Shift+Enter=newline) ")
    } else if line_count > 1 {
        " Message (Enter=send • Shift+Enter=newline) ".to_string()
    } else {
        " Message (Enter to send, Shift+Enter=newline, /help for commands) ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default().fg(if app.tab().is_running { Color::Rgb(251, 191, 36) } else { theme.fg }),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let is_empty = app.tab().input.text.is_empty() && !app.tab().is_running;
    let text = if is_empty {
        "Type your message here…".to_string()
    } else {
        app.tab().input.text.clone()
    };
    let style = if is_empty { theme.input_placeholder } else { theme.input };

    // Scroll offset so the cursor line stays visible when content exceeds the visible height.
    let (cursor_line, cursor_col) = app.tab().input.cursor_line_col();
    let visible_rows = inner.height as usize;
    let scroll_y: u16 = if cursor_line >= visible_rows {
        (cursor_line + 1 - visible_rows) as u16
    } else {
        0
    };

    let paragraph = Paragraph::new(text)
        .style(style)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    frame.render_widget(paragraph, inner);

    // Position the cursor at (cursor_col, cursor_line - scroll_y) within `inner`.
    if !app.tab().is_running && !is_empty {
        let screen_y = inner.y + (cursor_line as u16).saturating_sub(scroll_y);
        let screen_x = inner.x + cursor_col as u16;
        if screen_x < inner.x + inner.width && screen_y < inner.y + inner.height {
            frame.set_cursor_position((screen_x, screen_y));
        }
    } else if !app.tab().is_running && is_empty {
        // Empty input — cursor at top-left.
        if inner.width > 0 && inner.height > 0 {
            frame.set_cursor_position((inner.x, inner.y));
        }
    }

    // Show suggestions
    if !app.tab().input.suggestions.is_empty() {
        let frame_area = frame.area();
        let max_visible = (area.y.saturating_sub(1)) as usize; // space above input
        let visible_count = app.tab().input.suggestions.len().min(max_visible);
        if visible_count == 0 {
            // no room to show suggestions
        } else {
        let popup_height = (visible_count as u16 + 2).min(area.y); // +2 for border, never taller than space above
        let popup_width = area.width.min(60).min(frame_area.width.saturating_sub(area.x + 1));
        let popup_y = area.y.saturating_sub(popup_height);
        let popup_x = area.x + 1;
        // Final clip: ensure the rect fits entirely inside frame buffer
        if popup_width == 0 || popup_height == 0
            || popup_x >= frame_area.width
            || popup_y.saturating_add(popup_height) > frame_area.height
            || popup_x.saturating_add(popup_width) > frame_area.width
        {
            return;
        }
        let suggestions_area = Rect {
            x: popup_x,
            y: popup_y,
            width: popup_width,
            height: popup_height,
        };

        let items: Vec<Line> = app.tab().input.suggestions.iter().take(visible_count).enumerate().map(|(i, s)| {
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
        } // visible_count > 0
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let tab = app.tab();
    let session_name = if tab.name.is_empty() { format!("Session {}", app.active_tab + 1) } else { tab.name.clone() };
    let mut spans: Vec<Span> = vec![
        Span::styled(" ", Style::default()),
        Span::styled(&tab.status, theme.status_text),
        Span::styled(
            format!("  │  {}  │  Turn {}/{}  │  Model: {}  │  msgs {}  │  v{}",
                session_name,
                tab.turn,
                app.config.max_turns,
                tab.current_model,
                tab.messages.len(),
                env!("CARGO_PKG_VERSION"),
            ),
            theme.footer,
        ),
    ];

    // Memory status (P/B/H counts) — only show if brain_status has been loaded.
    if let Some(status) = &app.brain_status {
        let dim = Color::Rgb(100, 116, 139);
        let hot = Color::Rgb(74, 222, 128);
        let cold = Color::Rgb(71, 85, 105);
        let p_col = if status.project_entries > 0 { hot } else { cold };
        let b_col = if status.brain_entries > 0 { hot } else { cold };
        let h_col = if status.hive_entries > 0 { hot } else { cold };
        spans.push(Span::styled("  │  mem ", Style::default().fg(dim)));
        spans.push(Span::styled("P:", Style::default().fg(dim)));
        spans.push(Span::styled(format!("{}", status.project_entries), Style::default().fg(p_col)));
        spans.push(Span::styled(" B:", Style::default().fg(dim)));
        spans.push(Span::styled(format!("{}", status.brain_entries), Style::default().fg(b_col)));
        spans.push(Span::styled(" H:", Style::default().fg(dim)));
        spans.push(Span::styled(format!("{}", status.hive_entries), Style::default().fg(h_col)));
    }
    let footer = Line::from(spans);

    frame.render_widget(
        Paragraph::new(footer).style(Style::default().bg(Color::Rgb(30, 41, 59))),
        area,
    );
}
