//! Taylor resource yielding — activity modes and level tracking.
//!
//! When Venkat is actively using Taylor (the Mac Studio), ForgeFleet
//! reduces compute load on that machine. This module defines the
//! yield modes and activity detection signals.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ─── Yield Modes ─────────────────────────────────────────────────────────────

/// Taylor's resource yield mode.
///
/// Determines how much compute ForgeFleet is allowed to use on Taylor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum YieldMode {
    /// Venkat is actively using the machine — minimal compute jobs.
    /// Only lightweight tasks (chat responses, notifications).
    Interactive,

    /// Chat/voice session is active but machine is otherwise available.
    /// Medium load okay — single model inference, light processing.
    Assist,

    /// Machine is idle — Taylor rejoins the worker pool fully.
    /// Heavy jobs, multi-model inference, training all allowed.
    #[default]
    Idle,

    /// Manually set by Venkat — absolutely no heavy jobs.
    /// Only critical system tasks (heartbeat, health checks).
    Protected,
}

impl YieldMode {
    /// Returns the maximum number of concurrent inference jobs allowed.
    pub fn max_concurrent_jobs(&self) -> u32 {
        match self {
            Self::Interactive => 0, // No inference jobs.
            Self::Assist => 1,      // One at a time.
            Self::Idle => 4,        // Full capacity.
            Self::Protected => 0,   // Nothing.
        }
    }

    /// Returns the maximum GPU memory percentage ForgeFleet can use.
    pub fn max_gpu_percent(&self) -> u8 {
        match self {
            Self::Interactive => 10,
            Self::Assist => 40,
            Self::Idle => 90,
            Self::Protected => 0,
        }
    }

    /// Returns the maximum CPU percentage ForgeFleet can use.
    pub fn max_cpu_percent(&self) -> u8 {
        match self {
            Self::Interactive => 15,
            Self::Assist => 50,
            Self::Idle => 90,
            Self::Protected => 5,
        }
    }

    /// Whether this mode should accept new inference requests.
    pub fn accepts_inference(&self) -> bool {
        matches!(self, Self::Assist | Self::Idle)
    }

    /// Whether this is a human-active mode (Interactive or Assist).
    pub fn is_human_active(&self) -> bool {
        matches!(self, Self::Interactive | Self::Assist)
    }
}

impl std::fmt::Display for YieldMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interactive => write!(f, "Interactive (human active)"),
            Self::Assist => write!(f, "Assist (chat active)"),
            Self::Idle => write!(f, "Idle (full capacity)"),
            Self::Protected => write!(f, "Protected (manual lockout)"),
        }
    }
}

// ─── Activity Signals ────────────────────────────────────────────────────────

/// Signals monitored to determine Taylor's yield mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivitySignals {
    /// Keyboard/mouse activity detected in the last N seconds.
    pub input_active: bool,
    /// Seconds since last keyboard/mouse input.
    pub idle_seconds: u64,
    /// Current CPU usage percentage (0–100).
    pub cpu_percent: f32,
    /// Current memory usage percentage (0–100).
    pub memory_percent: f32,
    /// Current GPU usage percentage (0–100).
    pub gpu_percent: f32,
    /// Whether a foreground app is actively in use (not screensaver/lock).
    pub foreground_app_active: bool,
    /// Name of the foreground application (if detectable).
    pub foreground_app: Option<String>,
    /// Whether an active voice/chat session exists.
    pub chat_session_active: bool,
    /// Timestamp when these signals were sampled.
    pub sampled_at: DateTime<Utc>,
}

impl ActivitySignals {
    /// Create a new snapshot with current timestamp.
    pub fn new() -> Self {
        Self {
            input_active: false,
            idle_seconds: 0,
            cpu_percent: 0.0,
            memory_percent: 0.0,
            gpu_percent: 0.0,
            foreground_app_active: false,
            foreground_app: None,
            chat_session_active: false,
            sampled_at: Utc::now(),
        }
    }
}

impl Default for ActivitySignals {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Activity Level ──────────────────────────────────────────────────────────

/// Computed activity level from signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityLevel {
    /// No human activity — machine is idle.
    None = 0,
    /// Minimal activity (background apps, occasional input).
    Low = 1,
    /// Moderate activity (some typing, browsing).
    Medium = 2,
    /// Heavy activity (coding, video calls, gaming).
    High = 3,
}

impl std::fmt::Display for ActivityLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "None"),
            Self::Low => write!(f, "Low"),
            Self::Medium => write!(f, "Medium"),
            Self::High => write!(f, "High"),
        }
    }
}

// ─── Mode Resolution ─────────────────────────────────────────────────────────

/// Configuration for activity thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityThresholds {
    /// Seconds of idle before dropping to Idle mode.
    pub idle_timeout_secs: u64,
    /// CPU percentage above which we consider the machine "busy".
    pub cpu_busy_percent: f32,
    /// GPU percentage above which we consider the machine "busy".
    pub gpu_busy_percent: f32,
}

impl Default for ActivityThresholds {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 300, // 5 minutes
            cpu_busy_percent: 50.0,
            gpu_busy_percent: 40.0,
        }
    }
}

/// Determine the activity level from signals.
pub fn compute_activity_level(signals: &ActivitySignals) -> ActivityLevel {
    if signals.input_active && signals.cpu_percent > 60.0 {
        ActivityLevel::High
    } else if signals.input_active {
        ActivityLevel::Medium
    } else if signals.idle_seconds < 60 {
        ActivityLevel::Low
    } else {
        ActivityLevel::None
    }
}

/// Resolve the yield mode from signals and thresholds.
///
/// This is the core logic that decides how Taylor behaves.
/// Note: `Protected` mode is always set manually and overrides this.
pub fn resolve_yield_mode(
    signals: &ActivitySignals,
    thresholds: &ActivityThresholds,
    manual_override: Option<YieldMode>,
) -> YieldMode {
    // Manual override always wins.
    if let Some(mode) = manual_override {
        return mode;
    }

    let activity = compute_activity_level(signals);

    match activity {
        ActivityLevel::High => YieldMode::Interactive,
        ActivityLevel::Medium => {
            if signals.chat_session_active {
                YieldMode::Assist
            } else {
                YieldMode::Interactive
            }
        }
        ActivityLevel::Low => {
            if signals.chat_session_active {
                YieldMode::Assist
            } else if signals.idle_seconds > thresholds.idle_timeout_secs / 2 {
                YieldMode::Idle
            } else {
                YieldMode::Assist
            }
        }
        ActivityLevel::None => {
            if signals.idle_seconds >= thresholds.idle_timeout_secs {
                YieldMode::Idle
            } else {
                YieldMode::Assist
            }
        }
    }
}

/// Full activity state snapshot for a Taylor node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityState {
    /// Current yield mode.
    pub mode: YieldMode,
    /// Current activity level.
    pub level: ActivityLevel,
    /// Latest signals.
    pub signals: ActivitySignals,
    /// Manual override (if set).
    pub manual_override: Option<YieldMode>,
    /// When the mode last changed.
    pub mode_changed_at: DateTime<Utc>,
}

impl ActivityState {
    /// Create initial state (Idle, no activity).
    pub fn initial() -> Self {
        Self {
            mode: YieldMode::Idle,
            level: ActivityLevel::None,
            signals: ActivitySignals::new(),
            manual_override: None,
            mode_changed_at: Utc::now(),
        }
    }

    /// Update state with new signals.
    pub fn update(&mut self, signals: ActivitySignals, thresholds: &ActivityThresholds) {
        let new_level = compute_activity_level(&signals);
        let new_mode = resolve_yield_mode(&signals, thresholds, self.manual_override);

        if new_mode != self.mode {
            self.mode_changed_at = Utc::now();
        }

        self.level = new_level;
        self.mode = new_mode;
        self.signals = signals;
    }

    /// Set a manual override.
    pub fn set_manual_override(&mut self, mode: YieldMode) {
        self.manual_override = Some(mode);
        self.mode = mode;
        self.mode_changed_at = Utc::now();
    }

    /// Clear manual override — activity signals resume control.
    pub fn clear_manual_override(&mut self) {
        self.manual_override = None;
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yield_mode_defaults() {
        assert_eq!(YieldMode::default(), YieldMode::Idle);
    }

    #[test]
    fn test_yield_mode_constraints() {
        assert_eq!(YieldMode::Interactive.max_concurrent_jobs(), 0);
        assert_eq!(YieldMode::Assist.max_concurrent_jobs(), 1);
        assert_eq!(YieldMode::Idle.max_concurrent_jobs(), 4);
        assert_eq!(YieldMode::Protected.max_concurrent_jobs(), 0);

        assert!(YieldMode::Idle.accepts_inference());
        assert!(YieldMode::Assist.accepts_inference());
        assert!(!YieldMode::Interactive.accepts_inference());
        assert!(!YieldMode::Protected.accepts_inference());
    }

    #[test]
    fn test_activity_level_ordering() {
        assert!(ActivityLevel::None < ActivityLevel::Low);
        assert!(ActivityLevel::Low < ActivityLevel::Medium);
        assert!(ActivityLevel::Medium < ActivityLevel::High);
    }

    #[test]
    fn test_resolve_idle() {
        let signals = ActivitySignals {
            input_active: false,
            idle_seconds: 600, // 10 min
            cpu_percent: 5.0,
            memory_percent: 30.0,
            gpu_percent: 0.0,
            foreground_app_active: false,
            foreground_app: None,
            chat_session_active: false,
            sampled_at: Utc::now(),
        };
        let thresholds = ActivityThresholds::default();
        let mode = resolve_yield_mode(&signals, &thresholds, None);
        assert_eq!(mode, YieldMode::Idle);
    }

    #[test]
    fn test_resolve_interactive() {
        let signals = ActivitySignals {
            input_active: true,
            idle_seconds: 0,
            cpu_percent: 75.0,
            memory_percent: 60.0,
            gpu_percent: 50.0,
            foreground_app_active: true,
            foreground_app: Some("VS Code".into()),
            chat_session_active: false,
            sampled_at: Utc::now(),
        };
        let thresholds = ActivityThresholds::default();
        let mode = resolve_yield_mode(&signals, &thresholds, None);
        assert_eq!(mode, YieldMode::Interactive);
    }

    #[test]
    fn test_resolve_assist_with_chat() {
        let signals = ActivitySignals {
            input_active: true,
            idle_seconds: 0,
            cpu_percent: 20.0,
            memory_percent: 40.0,
            gpu_percent: 10.0,
            foreground_app_active: true,
            foreground_app: Some("Terminal".into()),
            chat_session_active: true,
            sampled_at: Utc::now(),
        };
        let thresholds = ActivityThresholds::default();
        let mode = resolve_yield_mode(&signals, &thresholds, None);
        assert_eq!(mode, YieldMode::Assist);
    }

    #[test]
    fn test_manual_override() {
        let signals = ActivitySignals {
            input_active: false,
            idle_seconds: 1000,
            cpu_percent: 0.0,
            memory_percent: 20.0,
            gpu_percent: 0.0,
            foreground_app_active: false,
            foreground_app: None,
            chat_session_active: false,
            sampled_at: Utc::now(),
        };
        let thresholds = ActivityThresholds::default();
        // Even though idle, manual override sets Protected.
        let mode = resolve_yield_mode(&signals, &thresholds, Some(YieldMode::Protected));
        assert_eq!(mode, YieldMode::Protected);
    }

    #[test]
    fn test_activity_state_update() {
        let mut state = ActivityState::initial();
        assert_eq!(state.mode, YieldMode::Idle);

        // Simulate user becoming active.
        let signals = ActivitySignals {
            input_active: true,
            idle_seconds: 0,
            cpu_percent: 80.0,
            memory_percent: 50.0,
            gpu_percent: 0.0,
            foreground_app_active: true,
            foreground_app: Some("Safari".into()),
            chat_session_active: false,
            sampled_at: Utc::now(),
        };
        state.update(signals, &ActivityThresholds::default());
        assert_eq!(state.mode, YieldMode::Interactive);
        assert_eq!(state.level, ActivityLevel::High);
    }

    #[test]
    fn test_activity_state_manual() {
        let mut state = ActivityState::initial();
        state.set_manual_override(YieldMode::Protected);
        assert_eq!(state.mode, YieldMode::Protected);
        assert_eq!(state.manual_override, Some(YieldMode::Protected));

        state.clear_manual_override();
        assert_eq!(state.manual_override, None);
    }

    #[test]
    fn test_serialize_yield_mode() {
        let json = serde_json::to_string(&YieldMode::Assist).unwrap();
        assert_eq!(json, r#""assist""#);
        let rt: YieldMode = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, YieldMode::Assist);
    }

    #[test]
    fn test_serialize_activity_level() {
        let json = serde_json::to_string(&ActivityLevel::High).unwrap();
        assert_eq!(json, r#""high""#);
        let rt: ActivityLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(rt, ActivityLevel::High);
    }
}
