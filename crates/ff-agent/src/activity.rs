use ff_core::ActivityLevel;
use ff_discovery::ActivitySignals;

pub fn decide_activity_level(
    signals: &ActivitySignals,
    override_level: Option<ActivityLevel>,
) -> ActivityLevel {
    if let Some(level) = override_level {
        return level;
    }

    let idle_seconds = signals.user_idle_seconds.unwrap_or(600);
    let cpu_pressure = signals.cpu_pressure_percent.unwrap_or(0.0);

    if idle_seconds < 30 {
        ActivityLevel::Interactive
    } else if idle_seconds < 240 || cpu_pressure > 70.0 {
        ActivityLevel::Assist
    } else {
        ActivityLevel::Idle
    }
}

pub fn should_yield_resources(level: ActivityLevel) -> bool {
    matches!(level, ActivityLevel::Interactive | ActivityLevel::Protected)
}
