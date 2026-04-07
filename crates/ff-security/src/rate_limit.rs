use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitSpec {
    pub max_requests: u32,
    pub window_secs: i64,
}

impl LimitSpec {
    pub fn window(&self) -> Duration {
        Duration::seconds(self.window_secs)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub per_user: LimitSpec,
    pub per_session: LimitSpec,
    pub per_tool: LimitSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitContext {
    pub user_id: String,
    pub session_id: String,
    pub tool: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitDecision {
    pub allowed: bool,
    pub exceeded_scope: Option<RateLimitScope>,
    pub retry_after_secs: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    User,
    Session,
    Tool,
}

#[derive(Debug, Clone)]
struct WindowCounter {
    start: DateTime<Utc>,
    count: u32,
}

/// Fixed-window rate limiter with per-user/per-session/per-tool scopes.
#[derive(Debug)]
pub struct RateLimiter {
    config: RateLimitConfig,
    user_counters: DashMap<String, WindowCounter>,
    session_counters: DashMap<String, WindowCounter>,
    tool_counters: DashMap<String, WindowCounter>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            user_counters: DashMap::new(),
            session_counters: DashMap::new(),
            tool_counters: DashMap::new(),
        }
    }

    pub fn check_and_record(&self, ctx: &RateLimitContext) -> RateLimitDecision {
        self.check_and_record_at(ctx, Utc::now())
    }

    pub fn check_and_record_at(
        &self,
        ctx: &RateLimitContext,
        now: DateTime<Utc>,
    ) -> RateLimitDecision {
        if let Some(retry_after) = would_exceed(
            &self.user_counters,
            &ctx.user_id,
            &self.config.per_user,
            now,
        ) {
            return RateLimitDecision {
                allowed: false,
                exceeded_scope: Some(RateLimitScope::User),
                retry_after_secs: Some(retry_after),
            };
        }

        if let Some(retry_after) = would_exceed(
            &self.session_counters,
            &ctx.session_id,
            &self.config.per_session,
            now,
        ) {
            return RateLimitDecision {
                allowed: false,
                exceeded_scope: Some(RateLimitScope::Session),
                retry_after_secs: Some(retry_after),
            };
        }

        if let Some(retry_after) =
            would_exceed(&self.tool_counters, &ctx.tool, &self.config.per_tool, now)
        {
            return RateLimitDecision {
                allowed: false,
                exceeded_scope: Some(RateLimitScope::Tool),
                retry_after_secs: Some(retry_after),
            };
        }

        increment_counter(
            &self.user_counters,
            &ctx.user_id,
            &self.config.per_user,
            now,
        );
        increment_counter(
            &self.session_counters,
            &ctx.session_id,
            &self.config.per_session,
            now,
        );
        increment_counter(&self.tool_counters, &ctx.tool, &self.config.per_tool, now);

        RateLimitDecision {
            allowed: true,
            exceeded_scope: None,
            retry_after_secs: None,
        }
    }
}

fn would_exceed(
    map: &DashMap<String, WindowCounter>,
    key: &str,
    spec: &LimitSpec,
    now: DateTime<Utc>,
) -> Option<i64> {
    if spec.max_requests == 0 {
        return Some(spec.window_secs.max(0));
    }

    let window = spec.window();
    if let Some(counter) = map.get(key) {
        let elapsed = now - counter.start;
        if elapsed < window && counter.count >= spec.max_requests {
            let retry_after = (window - elapsed).num_seconds().max(1);
            return Some(retry_after);
        }
    }

    None
}

fn increment_counter(
    map: &DashMap<String, WindowCounter>,
    key: &str,
    spec: &LimitSpec,
    now: DateTime<Utc>,
) {
    let window = spec.window();

    if let Some(mut entry) = map.get_mut(key) {
        if now - entry.start >= window {
            *entry = WindowCounter {
                start: now,
                count: 1,
            };
        } else {
            entry.count += 1;
        }
        return;
    }

    map.insert(
        key.to_string(),
        WindowCounter {
            start: now,
            count: 1,
        },
    );
}
