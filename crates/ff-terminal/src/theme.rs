//! Color theme for ForgeFleet Terminal.
//!
//! Palette mirrors the web dashboard CSS custom properties so the TUI and
//! dashboard share a uniform visual identity.

use ratatui::style::{Color, Modifier, Style};

/// ForgeFleet terminal color theme.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // ── Semantic dashboard palette ─────────────────────────────────────────
    pub bg: Color,
    pub surface: Color,
    pub panel: Color,
    pub elevated: Color,
    pub fg: Color,
    pub muted: Color,
    pub dim: Color,
    pub border: Color,
    pub border_subtle: Color,
    pub primary: Color,
    pub primary_muted: Color,
    pub primary_subtle: Color,

    // ── Status colors ──────────────────────────────────────────────────────
    pub status_ok: Color,
    pub status_warn: Color,
    pub status_crit: Color,
    pub status_info: Color,

    // ── Legacy aliases (kept for existing chat rendering) ──────────────────
    pub border_focused: Color,
    pub user_msg: Style,
    pub assistant_msg: Style,
    pub tool_start: Style,
    pub tool_end_ok: Style,
    pub tool_end_err: Style,
    pub status_text: Style,
    pub error_text: Style,
    pub header: Style,
    pub footer: Style,
    pub input: Style,
    pub input_placeholder: Style,
    pub fleet_online: Style,
    pub fleet_offline: Style,
    pub token_bar_low: Color,
    pub token_bar_mid: Color,
    pub token_bar_high: Color,
    pub command_hint: Style,
    pub code_block: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            // Dashboard semantic palette
            bg: Color::Rgb(2, 6, 23),                 // --background
            surface: Color::Rgb(15, 23, 42),          // --surface
            panel: Color::Rgb(30, 41, 59),            // --panel
            elevated: Color::Rgb(51, 65, 85),         // --elevated
            fg: Color::Rgb(226, 232, 240),            // --foreground
            muted: Color::Rgb(148, 163, 184),         // --muted
            dim: Color::Rgb(100, 116, 139),           // --dim
            border: Color::Rgb(51, 65, 85),           // --border
            border_subtle: Color::Rgb(71, 85, 105),   // --border-subtle
            primary: Color::Rgb(99, 102, 241),        // --primary
            primary_muted: Color::Rgb(129, 140, 248), // --primary-muted
            primary_subtle: Color::Rgb(30, 27, 75),   // --primary-subtle

            status_ok: Color::Rgb(74, 222, 128),
            status_warn: Color::Rgb(251, 191, 36),
            status_crit: Color::Rgb(248, 113, 113),
            status_info: Color::Rgb(125, 211, 252),

            // Legacy aliases
            border_focused: Color::Rgb(99, 102, 241),
            user_msg: Style::default().fg(Color::Rgb(125, 211, 252)),
            assistant_msg: Style::default().fg(Color::Rgb(134, 239, 172)),
            tool_start: Style::default().fg(Color::Rgb(251, 191, 36)),
            tool_end_ok: Style::default().fg(Color::Rgb(74, 222, 128)),
            tool_end_err: Style::default().fg(Color::Rgb(248, 113, 113)),
            status_text: Style::default().fg(Color::Rgb(148, 163, 184)),
            error_text: Style::default().fg(Color::Rgb(248, 113, 113)),
            header: Style::default()
                .fg(Color::Rgb(226, 232, 240))
                .add_modifier(Modifier::BOLD),
            footer: Style::default().fg(Color::Rgb(100, 116, 139)),
            input: Style::default().fg(Color::Rgb(226, 232, 240)),
            input_placeholder: Style::default().fg(Color::Rgb(71, 85, 105)),
            fleet_online: Style::default().fg(Color::Rgb(74, 222, 128)),
            fleet_offline: Style::default().fg(Color::Rgb(248, 113, 113)),
            token_bar_low: Color::Rgb(74, 222, 128),
            token_bar_mid: Color::Rgb(251, 191, 36),
            token_bar_high: Color::Rgb(248, 113, 113),
            command_hint: Style::default().fg(Color::Rgb(139, 92, 246)),
            code_block: Style::default().fg(Color::Rgb(196, 181, 253)),
        }
    }
}

impl Theme {
    /// Style for a subtle block title.
    pub fn title_style(&self) -> Style {
        Style::default().fg(self.fg).add_modifier(Modifier::BOLD)
    }

    /// Style for dim helper text inside blocks.
    pub fn help_style(&self) -> Style {
        Style::default().fg(self.dim)
    }

    /// Style for an active/selected list item.
    pub fn selected_style(&self) -> Style {
        Style::default().fg(Color::White).bg(self.primary)
    }

    /// Color for a named status.
    pub fn status_color(&self, status: &str) -> Color {
        match status.to_lowercase().as_str() {
            "ok" | "online" | "success" | "ready" | "healthy" | "enabled" => self.status_ok,
            "warn" | "warning" | "degraded" => self.status_warn,
            "crit" | "critical" | "error" | "offline" | "down" | "disabled" => self.status_crit,
            _ => self.dim,
        }
    }
}
