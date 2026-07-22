pub mod dispatch;
pub mod pm;

pub mod styles {
    pub const TOKENS_CSS: &str = include_str!("styles/tokens.css");

    pub const FONT_ASSET_BASE: &str = "/assets/fonts/";
    pub const INTER_VARIABLE_FONT: &str = "/assets/fonts/Inter-Variable.woff2";
    pub const JETBRAINS_MONO_VARIABLE_FONT: &str = "/assets/fonts/JetBrainsMono-Variable.woff2";

    pub const FONT_ASSET_PATHS: &[&str] = &[INTER_VARIABLE_FONT, JETBRAINS_MONO_VARIABLE_FONT];
}
