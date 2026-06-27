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

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(max: u32, window: i64) -> LimitSpec {
        LimitSpec {
            max_requests: max,
            window_secs: window,
        }
    }

    fn config(user: u32, session: u32, tool: u32, window: i64) -> RateLimitConfig {
        RateLimitConfig {
            per_user: spec(user, window),
            per_session: spec(session, window),
            per_tool: spec(tool, window),
        }
    }

    fn ctx(user: &str, session: &str, tool: &str) -> RateLimitContext {
        RateLimitContext {
            user_id: user.into(),
            session_id: session.into(),
            tool: tool.into(),
        }
    }

    #[test]
    fn allows_up_to_max_then_blocks() {
        let rl = RateLimiter::new(config(3, 100, 100, 60));
        let t0 = Utc::now();
        let c = ctx("u", "s", "t");
        for i in 0..3 {
            let d = rl.check_and_record_at(&c, t0);
            assert!(d.allowed, "request {i} should be allowed");
        }
        let d = rl.check_and_record_at(&c, t0);
        assert!(!d.allowed);
        assert_eq!(d.exceeded_scope, Some(RateLimitScope::User));
        let retry = d.retry_after_secs.unwrap();
        assert!(
            (1..=60).contains(&retry),
            "retry_after {retry} out of window"
        );
    }

    #[test]
    fn window_resets_after_expiry() {
        let rl = RateLimiter::new(config(1, 100, 100, 60));
        let t0 = Utc::now();
        let c = ctx("u", "s", "t");
        assert!(rl.check_and_record_at(&c, t0).allowed);
        // Still inside the window — blocked.
        assert!(
            !rl.check_and_record_at(&c, t0 + Duration::seconds(59))
                .allowed
        );
        // At/after the window boundary — counter resets, allowed again.
        assert!(
            rl.check_and_record_at(&c, t0 + Duration::seconds(60))
                .allowed
        );
    }

    #[test]
    fn attributes_the_blocking_scope() {
        // Session is the tightest scope here.
        let rl = RateLimiter::new(config(100, 1, 100, 60));
        let t0 = Utc::now();
        let c = ctx("u", "s", "t");
        assert!(rl.check_and_record_at(&c, t0).allowed);
        let d = rl.check_and_record_at(&c, t0);
        assert!(!d.allowed);
        assert_eq!(d.exceeded_scope, Some(RateLimitScope::Session));
    }

    #[test]
    fn zero_max_always_blocks() {
        let rl = RateLimiter::new(config(0, 100, 100, 30));
        let d = rl.check_and_record_at(&ctx("u", "s", "t"), Utc::now());
        assert!(!d.allowed);
        assert_eq!(d.exceeded_scope, Some(RateLimitScope::User));
    }

    #[test]
    fn rejected_request_does_not_consume_other_scope_quota() {
        // tool caps at 1; user caps at 2. A request blocked on the tool scope
        // must NOT have incremented the user counter — otherwise the user's
        // quota would leak away on rejected requests.
        let rl = RateLimiter::new(config(2, 100, 1, 60));
        let t0 = Utc::now();
        // req1: tool "t" — allowed (user=1, tool[t]=1).
        assert!(rl.check_and_record_at(&ctx("u", "s", "t"), t0).allowed);
        // req2: tool "t" again — blocked on Tool. user must stay at 1.
        let d = rl.check_and_record_at(&ctx("u", "s", "t"), t0);
        assert_eq!(d.exceeded_scope, Some(RateLimitScope::Tool));
        // req3: fresh tool "t2" — allowed (user=2).
        assert!(rl.check_and_record_at(&ctx("u", "s", "t2"), t0).allowed);
        // req4: fresh tool "t3" — now the USER cap of 2 is hit, proving req2
        // (tool-blocked) never consumed user quota.
        let d = rl.check_and_record_at(&ctx("u", "s", "t3"), t0);
        assert!(!d.allowed);
        assert_eq!(d.exceeded_scope, Some(RateLimitScope::User));
    }
}
