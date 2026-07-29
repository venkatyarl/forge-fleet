//! ratatui rendering: transcript pane, input box, status bar.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, Item};
use crate::markdown;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn item_lines(item: &Item) -> Vec<Line<'static>> {
    match item {
        Item::User(text) => {
            let mut lines = vec![Line::from(Span::styled(
                "you",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ))];
            for l in text.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(Color::Blue),
                )));
            }
            lines.push(Line::default());
            lines
        }
        Item::Assistant(text) => markdown::render(text),
        Item::Tool {
            name,
            summary,
            done,
        } => {
            let (mark, style) = match done {
                None => ("…", Style::default().fg(Color::Yellow)),
                Some((false, _)) => ("✓", Style::default().fg(Color::Green)),
                Some((true, _)) => ("✗", Style::default().fg(Color::Red)),
            };
            let tail = match done {
                Some((_, ms)) => format!(" ({:.1}s)", *ms as f64 / 1000.0),
                None => String::new(),
            };
            vec![Line::from(vec![
                Span::styled(format!("⚙ {name} "), Style::default().fg(Color::Magenta)),
                Span::styled(summary.clone(), Style::default().fg(Color::DarkGray)),
                Span::styled(format!(" {mark}{tail}"), style),
            ])]
        }
        Item::Note(text) => text
            .lines()
            .map(|l| {
                Line::from(Span::styled(
                    format!("· {l}"),
                    Style::default().fg(Color::DarkGray),
                ))
            })
            .collect(),
        Item::Error(text) => text
            .lines()
            .map(|l| {
                Line::from(Span::styled(
                    format!("✗ {l}"),
                    Style::default().fg(Color::Red),
                ))
            })
            .collect(),
    }
}

pub fn render(f: &mut Frame, app: &App) {
    let input_height = (app.input.lines().count() as u16 + 2).clamp(3, 9);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(f.area());
    let (transcript_area, input_area, status_area) = (chunks[0], chunks[1], chunks[2]);

    // Transcript: flatten items to lines, pin to bottom unless scrolled up.
    let width = transcript_area.width.saturating_sub(1).max(10) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for item in &app.items {
        lines.extend(item_lines(item));
        lines.push(Line::default());
    }
    // Rough wrapped-height estimate so we can pin to the bottom.
    let est_wrapped: usize = lines
        .iter()
        .map(|l| {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            (w / width) + 1
        })
        .sum();
    let viewport = transcript_area.height as usize;
    let bottom = est_wrapped.saturating_sub(viewport);
    let scroll = bottom.saturating_sub(app.scroll_up as usize);
    let para = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    f.render_widget(para, transcript_area);

    // Input box.
    let border = if app.running {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan)
    };
    let input = Paragraph::new(app.input.clone())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border)
                .title(" input "),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(input, input_area);
    // Cursor: place at end of the (wrapped) input tail. Guard every bound —
    // a zero-sized area (e.g. a winsize-less pty) must never underflow.
    let pre = &app.input;
    let last_line = pre.lines().last().unwrap_or("");
    let cur_y = input_area.y + 1 + pre.lines().count().saturating_sub(1) as u16;
    let cur_x = input_area.x + 1 + last_line.chars().count() as u16;
    let max_x = input_area.x + input_area.width.saturating_sub(2);
    let max_y = input_area.y + input_area.height.saturating_sub(1);
    if input_area.width >= 4 && input_area.height >= 2 && cur_y < max_y {
        f.set_cursor_position((cur_x.min(max_x), cur_y));
    }

    // Status bar.
    let state = if app.running {
        format!(
            "{} running · Esc abort",
            SPINNER[app.spinner_tick % SPINNER.len()]
        )
    } else {
        "idle".to_string()
    };
    let ctx = app
        .ctx_pct
        .map(|p| format!(" · ctx {p:.0}%"))
        .unwrap_or_default();
    let left = format!(" {} · turn {}{}", app.status, app.turn, ctx);
    let right = format!("{state} ");
    let gap = status_area
        .width
        .saturating_sub((left.chars().count() + right.chars().count()) as u16);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, Style::default().fg(Color::Black).bg(Color::Cyan)),
            Span::styled(" ".repeat(gap as usize), Style::default().bg(Color::Cyan)),
            Span::styled(
                right,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        status_area,
    );
}
