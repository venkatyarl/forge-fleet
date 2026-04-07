//! Color theme for ForgeFleet Terminal.

use ratatui::style::{Color, Modifier, Style};

/// ForgeFleet terminal color theme.
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub border: Color,
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
            bg: Color::Rgb(15, 23, 42),          // slate-900
            fg: Color::Rgb(226, 232, 240),        // slate-200
            border: Color::Rgb(51, 65, 85),       // slate-700
            border_focused: Color::Rgb(99, 102, 241), // indigo-500
            user_msg: Style::default().fg(Color::Rgb(125, 211, 252)),  // sky-300
            assistant_msg: Style::default().fg(Color::Rgb(134, 239, 172)), // emerald-300
            tool_start: Style::default().fg(Color::Rgb(251, 191, 36)), // amber-400
            tool_end_ok: Style::default().fg(Color::Rgb(74, 222, 128)), // green-400
            tool_end_err: Style::default().fg(Color::Rgb(248, 113, 113)), // red-400
            status_text: Style::default().fg(Color::Rgb(148, 163, 184)), // slate-400
            error_text: Style::default().fg(Color::Rgb(248, 113, 113)), // red-400
            header: Style::default().fg(Color::Rgb(226, 232, 240)).add_modifier(Modifier::BOLD),
            footer: Style::default().fg(Color::Rgb(100, 116, 139)), // slate-500
            input: Style::default().fg(Color::Rgb(226, 232, 240)),
            input_placeholder: Style::default().fg(Color::Rgb(71, 85, 105)), // slate-600
            fleet_online: Style::default().fg(Color::Rgb(74, 222, 128)),
            fleet_offline: Style::default().fg(Color::Rgb(248, 113, 113)),
            token_bar_low: Color::Rgb(74, 222, 128),  // green
            token_bar_mid: Color::Rgb(251, 191, 36),   // amber
            token_bar_high: Color::Rgb(248, 113, 113),  // red
            command_hint: Style::default().fg(Color::Rgb(139, 92, 246)), // violet-500
            code_block: Style::default().fg(Color::Rgb(196, 181, 253)), // violet-300
        }
    }
}
