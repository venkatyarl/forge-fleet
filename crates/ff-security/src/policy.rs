use serde::{Deserialize, Serialize};
use tracing::debug;
use uuid::Uuid;

/// A policy decision outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyEffect {
    Allow,
    Deny,
}

/// Request context evaluated against policy rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRequest {
    pub user_id: String,
    pub session_id: String,
    pub tool: String,
    pub path: String,
}

impl PolicyRequest {
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        tool: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            session_id: session_id.into(),
            tool: tool.into(),
            path: path.into(),
        }
    }
}

/// Rule selectors. `"*"` means wildcard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySelectors {
    pub users: Vec<String>,
    pub sessions: Vec<String>,
    pub tools: Vec<String>,
    /// Paths support exact, prefix (`"/tmp/**"`), and wildcard (`"*"`).
    pub paths: Vec<String>,
}

impl PolicySelectors {
    pub fn any() -> Self {
        Self {
            users: vec!["*".to_string()],
            sessions: vec!["*".to_string()],
            tools: vec!["*".to_string()],
            paths: vec!["*".to_string()],
        }
    }
}

/// A policy rule with explicit ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRule {
    pub id: Uuid,
    pub name: String,
    pub effect: PolicyEffect,
    /// Higher priority wins. If equal, insertion order is preserved.
    pub priority: i32,
    pub enabled: bool,
    pub selectors: PolicySelectors,
}

impl PolicyRule {
    pub fn new(
        name: impl Into<String>,
        effect: PolicyEffect,
        priority: i32,
        selectors: PolicySelectors,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            effect,
            priority,
            enabled: true,
            selectors,
        }
    }

    pub fn matches(&self, request: &PolicyRequest) -> bool {
        self.enabled
            && matches_selector(&self.selectors.users, &request.user_id)
            && matches_selector(&self.selectors.sessions, &request.session_id)
            && matches_selector(&self.selectors.tools, &request.tool)
            && matches_path_selector(&self.selectors.paths, &request.path)
    }
}

/// Final policy decision with optional matching rule metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub effect: PolicyEffect,
    pub rule_id: Option<Uuid>,
    pub rule_name: Option<String>,
    pub reason: String,
}

impl PolicyDecision {
    pub fn allowed(&self) -> bool {
        self.effect == PolicyEffect::Allow
    }
}

/// In-memory policy set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyEngine {
    rules: Vec<PolicyRule>,
}

impl PolicyEngine {
    pub fn new(mut rules: Vec<PolicyRule>) -> Self {
        rules.sort_by(|a, b| b.priority.cmp(&a.priority));
        Self { rules }
    }

    pub fn add_rule(&mut self, rule: PolicyRule) {
        self.rules.push(rule);
        self.rules.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    pub fn rules(&self) -> &[PolicyRule] {
        &self.rules
    }

    /// Evaluates request against rules. First matching rule wins.
    /// If no rule matches, default is deny.
    pub fn evaluate(&self, request: &PolicyRequest) -> PolicyDecision {
        for rule in &self.rules {
            if rule.matches(request) {
                debug!(
                    rule_id = %rule.id,
                    rule_name = %rule.name,
                    user_id = %request.user_id,
                    session_id = %request.session_id,
                    tool = %request.tool,
                    path = %request.path,
                    "policy rule matched"
                );

                return PolicyDecision {
                    effect: rule.effect,
                    rule_id: Some(rule.id),
                    rule_name: Some(rule.name.clone()),
                    reason: format!("matched rule '{}'", rule.name),
                };
            }
        }

        PolicyDecision {
            effect: PolicyEffect::Deny,
            rule_id: None,
            rule_name: None,
            reason: "default deny (no matching rule)".to_string(),
        }
    }
}

fn matches_selector(selector_values: &[String], value: &str) -> bool {
    selector_values
        .iter()
        .any(|candidate| candidate == "*" || candidate == value)
}

fn matches_path_selector(selector_values: &[String], path: &str) -> bool {
    selector_values.iter().any(|pattern| {
        if pattern == "*" {
            return true;
        }

        if let Some(prefix) = pattern.strip_suffix("/**") {
            return path == prefix || path.starts_with(&(prefix.to_string() + "/"));
        }

        pattern == path
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_rule_with_higher_priority_wins() {
        let allow_all =
            PolicyRule::new("allow all", PolicyEffect::Allow, 10, PolicySelectors::any());

        let deny_rm = PolicyRule::new(
            "deny rm tool",
            PolicyEffect::Deny,
            100,
            PolicySelectors {
                users: vec!["*".into()],
                sessions: vec!["*".into()],
                tools: vec!["rm".into()],
                paths: vec!["*".into()],
            },
        );

        let engine = PolicyEngine::new(vec![allow_all, deny_rm]);
        let req = PolicyRequest::new("u1", "s1", "rm", "/tmp/a.txt");

        let decision = engine.evaluate(&req);
        assert_eq!(decision.effect, PolicyEffect::Deny);
        assert!(decision.reason.contains("deny rm tool"));
    }

    #[test]
    fn path_prefix_rule_matches_subpaths() {
        let allow_workspace = PolicyRule::new(
            "allow workspace",
            PolicyEffect::Allow,
            50,
            PolicySelectors {
                users: vec!["*".into()],
                sessions: vec!["*".into()],
                tools: vec!["write".into()],
                paths: vec!["/workspace/**".into()],
            },
        );

        let engine = PolicyEngine::new(vec![allow_workspace]);
        let req = PolicyRequest::new("u1", "s1", "write", "/workspace/project/file.rs");

        let decision = engine.evaluate(&req);
        assert!(decision.allowed());
    }

    #[test]
    fn no_match_defaults_to_deny() {
        let engine = PolicyEngine::default();
        let req = PolicyRequest::new("u1", "s1", "read", "/tmp/anything");

        let decision = engine.evaluate(&req);
        assert_eq!(decision.effect, PolicyEffect::Deny);
        assert!(decision.reason.contains("default deny"));
    }
}
