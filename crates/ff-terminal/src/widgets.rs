//! Custom ratatui widgets for ForgeFleet Terminal.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

/// Token usage gauge bar.
pub struct TokenGauge {
    pub used: usize,
    pub total: usize,
    pub low_color: Color,
    pub mid_color: Color,
    pub high_color: Color,
}

impl Widget for TokenGauge {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 1 {
            return;
        }

        let pct = if self.total > 0 {
            (self.used as f64 / self.total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };

        let color = if pct < 50.0 {
            self.low_color
        } else if pct < 80.0 {
            self.mid_color
        } else {
            self.high_color
        };

        let bar_width = ((pct / 100.0) * (area.width as f64 - 2.0)) as u16;
        let label = format!("{:.0}% ({}/{})", pct, self.used, self.total);

        // Draw background
        for x in area.x..area.x + area.width {
            buf[(x, area.y)]
                .set_char('░')
                .set_fg(Color::Rgb(51, 65, 85));
        }

        // Draw filled portion
        for x in area.x..area.x + bar_width.min(area.width) {
            buf[(x, area.y)].set_char('█').set_fg(color);
        }

        // Draw label centered
        let label_start = area.x + (area.width.saturating_sub(label.len() as u16)) / 2;
        for (i, ch) in label.chars().enumerate() {
            let x = label_start + i as u16;
            if x < area.x + area.width {
                buf[(x, area.y)].set_char(ch).set_fg(Color::White);
            }
        }
    }
}

/// Fleet status indicator for a single node.
pub struct FleetNodeWidget {
    pub name: String,
    pub status: NodeDisplayStatus,
    pub model: String,
}

pub enum NodeDisplayStatus {
    Online,
    Busy,
    Offline,
}

impl Widget for FleetNodeWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 5 || area.height < 1 {
            return;
        }

        let (icon, color) = match self.status {
            NodeDisplayStatus::Online => ("●", Color::Rgb(74, 222, 128)),
            NodeDisplayStatus::Busy => ("◉", Color::Rgb(251, 191, 36)),
            NodeDisplayStatus::Offline => ("○", Color::Rgb(248, 113, 113)),
        };

        let text = format!("{icon} {:<10} {}", self.name, self.model);
        let truncated = if text.len() > area.width as usize {
            format!("{}…", &text[..area.width as usize - 1])
        } else {
            text
        };

        buf.set_string(area.x, area.y, &truncated, Style::default().fg(color));
    }
}

/// Tool execution card.
pub struct ToolCard {
    pub tool_name: String,
    pub status: ToolCardStatus,
    pub input_preview: String,
    pub output_preview: String,
    pub duration_ms: Option<u64>,
    pub expanded: bool,
}

pub enum ToolCardStatus {
    Running,
    Success,
    Error,
}

impl Widget for ToolCard {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < 2 {
            return;
        }

        let (icon, border_color) = match self.status {
            ToolCardStatus::Running => ("⚡", Color::Rgb(251, 191, 36)),
            ToolCardStatus::Success => ("✓", Color::Rgb(74, 222, 128)),
            ToolCardStatus::Error => ("✗", Color::Rgb(248, 113, 113)),
        };

        let duration_str = self
            .duration_ms
            .map(|ms| format!(" ({ms}ms)"))
            .unwrap_or_default();
        let header = format!("{icon} {}{duration_str}", self.tool_name);

        let block = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(border_color));

        let content = if self.expanded && !self.output_preview.is_empty() {
            format!("{header}\n{}", self.output_preview)
        } else {
            format!(
                "{header} {}",
                truncate(&self.input_preview, area.width as usize - header.len() - 2)
            )
        };

        let paragraph = Paragraph::new(content)
            .block(block)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Rgb(148, 163, 184)));

        paragraph.render(area, buf);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max > 3 {
        format!("{}...", &s[..max - 3])
    } else {
        s[..max].to_string()
    }
}
