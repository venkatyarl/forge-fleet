//! Lightweight markdown → ratatui renderer for the transcript pane.
//!
//! Covers the subset LLMs actually emit: fenced code blocks (with language
//! tint), ATX headers, bullet/numbered lists, blockquotes, horizontal rules,
//! and inline **bold** / *italic* / `code`. Deliberately dependency-free —
//! pulldown-cmark would be overkill and the terminal only needs styling, not
//! a DOM.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const CODE_FG: Color = Color::Green;
const CODE_BG: Color = Color::Rgb(30, 34, 42);
const HEADER: Color = Color::Cyan;
const QUOTE: Color = Color::DarkGray;

fn base() -> Style {
    Style::default()
}

fn code_style() -> Style {
    Style::default().fg(CODE_FG).bg(CODE_BG)
}

/// Render inline spans (bold/italic/inline-code) for one logical line.
fn inline(text: &str, style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text.to_string();
    // Order matters: code spans first (protect contents), then bold, then italic.
    while !rest.is_empty() {
        if let Some(start) = rest.find('`') {
            if let Some(end_rel) = rest[start + 1..].find('`') {
                let (before, tail) = rest.split_at(start);
                push_emphasis(before, style, &mut spans);
                let code = &tail[1..1 + end_rel];
                spans.push(Span::styled(format!(" {code} "), code_style()));
                rest = tail[1 + end_rel + 1..].to_string();
                continue;
            }
        }
        push_emphasis(&rest, style, &mut spans);
        break;
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), style));
    }
    spans
}

/// Bold/italic emphasis for a segment that contains no inline code.
fn push_emphasis(text: &str, style: Style, spans: &mut Vec<Span<'static>>) {
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(start) = rest.find("**") {
            if let Some(end_rel) = rest[start + 2..].find("**") {
                let (before, tail) = rest.split_at(start);
                if !before.is_empty() {
                    spans.push(Span::styled(before.to_string(), style));
                }
                spans.push(Span::styled(
                    tail[2..2 + end_rel].to_string(),
                    style.add_modifier(Modifier::BOLD),
                ));
                rest = &tail[2 + end_rel + 2..];
                continue;
            }
        }
        if let Some(start) = rest.find('*') {
            let after = &rest[start + 1..];
            // A leftover "**" was already tried above (no closer) — leave it
            // literal; and a closer must have non-empty content before it.
            if !after.starts_with('*')
                && let Some(end_rel) = after.find('*')
                && end_rel > 0
            {
                let (before, tail) = rest.split_at(start);
                if !before.is_empty() {
                    spans.push(Span::styled(before.to_string(), style));
                }
                spans.push(Span::styled(
                    tail[1..1 + end_rel].to_string(),
                    style.add_modifier(Modifier::ITALIC),
                ));
                rest = &tail[1 + end_rel + 1..];
                continue;
            }
        }
        spans.push(Span::styled(rest.to_string(), style));
        break;
    }
}

/// Render a markdown document into styled lines for the transcript.
pub fn render(md: &str) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_fence = false;

    for raw in md.lines() {
        let line = raw.to_string();
        let trimmed = line.trim_start().to_string();

        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            // Show the fence line dimmed (language hint) rather than hiding it.
            out.push(Line::from(Span::styled(line, Style::default().fg(QUOTE))));
            continue;
        }
        if in_fence {
            out.push(Line::from(Span::styled(format!("  {line}"), code_style())));
            continue;
        }

        if trimmed.starts_with('#') {
            let text = trimmed.trim_start_matches('#').trim().to_string();
            out.push(Line::from(Span::styled(
                text,
                Style::default().fg(HEADER).add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if trimmed == "---" || trimmed == "***" {
            out.push(Line::from(Span::styled(
                "─".repeat(40),
                Style::default().fg(QUOTE),
            )));
            continue;
        }
        if trimmed.starts_with('>') {
            let text = trimmed.trim_start_matches('>').trim().to_string();
            out.push(Line::from(Span::styled(
                format!("▎ {text}"),
                Style::default().fg(QUOTE).add_modifier(Modifier::ITALIC),
            )));
            continue;
        }
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let text = trimmed[2..].to_string();
            let mut spans = vec![Span::styled("  • ".to_string(), base())];
            spans.extend(inline(&text, base()));
            out.push(Line::from(spans));
            continue;
        }
        out.push(Line::from(inline(&line, base())));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn headers_bold_and_code_render() {
        let out = render("# Title\nSome **bold** and `code` here");
        let text = plain(&out);
        assert!(text.contains("Title"));
        assert!(text.contains("bold"));
        assert!(text.contains(" code "));
        // Header is bold-styled.
        assert!(out[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn fenced_code_block_is_tinted_and_literal() {
        let out = render("```rust\nfn main() { **not bold** }\n```");
        let text = plain(&out);
        assert!(text.contains("fn main() { **not bold** }"));
        assert_eq!(out[1].spans[0].style.bg, Some(CODE_BG));
    }

    #[test]
    fn bullets_and_quotes_render() {
        let out = render("- one\n- two\n> a note");
        let text = plain(&out);
        assert!(text.contains("  • one"));
        assert!(text.contains("▎ a note"));
    }

    #[test]
    fn unclosed_marks_degrade_to_literal() {
        let out = render("a **bold without end and `code without end");
        let text = plain(&out);
        assert!(text.contains("**bold without end"));
    }
}
