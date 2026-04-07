//! Agent role library — pre-built agent role templates with model/tool routing.
//!
//! ForgeFleet's role-based agent system for fleet-native execution.
//! Each role specifies preferred models, allowed tools, and system prompt customizations.

use serde::{Deserialize, Serialize};

/// A pre-built agent role template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRole {
    pub name: String,
    pub category: RoleCategory,
    pub description: String,
    /// Preferred model size (smallest acceptable).
    pub min_model_params: u64,
    /// Preferred model type.
    pub preferred_model_type: ModelPreference,
    /// Tools this role should have access to.
    pub allowed_tools: Vec<String>,
    /// Tools explicitly denied.
    pub denied_tools: Vec<String>,
    /// Additional system prompt instructions.
    pub system_prompt_extension: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleCategory {
    CoreDevelopment,
    Frontend,
    Backend,
    Infrastructure,
    Security,
    Testing,
    DataScience,
    Documentation,
    ProjectManagement,
    Research,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPreference {
    Coding,
    Reasoning,
    Fast,
    Large,
    Any,
}

/// Get all built-in agent roles.
pub fn builtin_roles() -> Vec<AgentRole> {
    vec![
        // Core Development
        AgentRole {
            name: "rust-developer".into(), category: RoleCategory::CoreDevelopment,
            description: "Expert Rust developer for systems programming, async, and performance".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Coding,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Write".into(), "Edit".into(), "Glob".into(), "Grep".into(), "Agent".into()],
            denied_tools: vec![],
            system_prompt_extension: "You are an expert Rust developer. Follow Rust idioms, use proper error handling with anyhow/thiserror, prefer async/await with tokio, write tests.".into(),
        },
        AgentRole {
            name: "typescript-developer".into(), category: RoleCategory::Frontend,
            description: "TypeScript/React developer for frontend and Node.js".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Coding,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Write".into(), "Edit".into(), "Glob".into(), "Grep".into()],
            denied_tools: vec![],
            system_prompt_extension: "You are an expert TypeScript developer. Use strict TypeScript, React best practices, proper types (no any), and modern patterns.".into(),
        },
        AgentRole {
            name: "python-developer".into(), category: RoleCategory::CoreDevelopment,
            description: "Python developer for scripting, data, and automation".into(),
            min_model_params: 14_000_000_000, preferred_model_type: ModelPreference::Coding,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Write".into(), "Edit".into(), "Glob".into(), "Grep".into()],
            denied_tools: vec![],
            system_prompt_extension: "You are an expert Python developer. Use type hints, follow PEP 8, prefer pathlib over os.path, use modern Python 3.12+ features.".into(),
        },
        // Security
        AgentRole {
            name: "security-auditor".into(), category: RoleCategory::Security,
            description: "Security reviewer analyzing code for vulnerabilities".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Reasoning,
            allowed_tools: vec!["Read".into(), "Glob".into(), "Grep".into(), "Bash".into(), "WebSearch".into()],
            denied_tools: vec!["Write".into(), "Edit".into()],
            system_prompt_extension: "You are a security auditor. Review code for OWASP top 10, injection vulnerabilities, auth issues, secrets exposure, and supply chain risks. Be thorough and specific.".into(),
        },
        // Testing
        AgentRole {
            name: "test-writer".into(), category: RoleCategory::Testing,
            description: "Writes comprehensive unit and integration tests".into(),
            min_model_params: 14_000_000_000, preferred_model_type: ModelPreference::Coding,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Write".into(), "Edit".into(), "Glob".into(), "Grep".into()],
            denied_tools: vec![],
            system_prompt_extension: "You are a test engineer. Write comprehensive tests: unit tests, integration tests, edge cases. Use the project's existing test framework and patterns.".into(),
        },
        AgentRole {
            name: "test-runner".into(), category: RoleCategory::Testing,
            description: "Runs test suites and reports results".into(),
            min_model_params: 9_000_000_000, preferred_model_type: ModelPreference::Fast,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Glob".into(), "Grep".into()],
            denied_tools: vec!["Write".into(), "Edit".into()],
            system_prompt_extension: "You are a test runner. Execute test suites, report pass/fail results clearly, identify flaky tests, suggest fixes for failures.".into(),
        },
        // Code Review
        AgentRole {
            name: "code-reviewer".into(), category: RoleCategory::CoreDevelopment,
            description: "Reviews code changes for quality, correctness, and style".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Reasoning,
            allowed_tools: vec!["Read".into(), "Glob".into(), "Grep".into(), "Bash".into()],
            denied_tools: vec!["Write".into(), "Edit".into()],
            system_prompt_extension: "You are a code reviewer. Review for correctness, performance, readability, and adherence to project conventions. Be constructive and specific.".into(),
        },
        // Infrastructure
        AgentRole {
            name: "devops-engineer".into(), category: RoleCategory::Infrastructure,
            description: "DevOps and infrastructure automation".into(),
            min_model_params: 14_000_000_000, preferred_model_type: ModelPreference::Coding,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Write".into(), "Edit".into(), "Glob".into(), "Grep".into(), "WebFetch".into()],
            denied_tools: vec![],
            system_prompt_extension: "You are a DevOps engineer. Handle Docker, CI/CD, deployment scripts, monitoring configs, and infrastructure automation.".into(),
        },
        AgentRole {
            name: "database-admin".into(), category: RoleCategory::Infrastructure,
            description: "Database schema design, migrations, and optimization".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Reasoning,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Write".into(), "Edit".into(), "Glob".into(), "Grep".into()],
            denied_tools: vec![],
            system_prompt_extension: "You are a database administrator. Design schemas, write migrations, optimize queries, handle indexing. Be careful with destructive SQL operations.".into(),
        },
        // Documentation
        AgentRole {
            name: "documentation-writer".into(), category: RoleCategory::Documentation,
            description: "Writes technical documentation, API docs, and guides".into(),
            min_model_params: 14_000_000_000, preferred_model_type: ModelPreference::Any,
            allowed_tools: vec!["Read".into(), "Write".into(), "Edit".into(), "Glob".into(), "Grep".into()],
            denied_tools: vec!["Bash".into()],
            system_prompt_extension: "You are a technical writer. Write clear, accurate documentation. Follow the project's existing doc style. Include examples.".into(),
        },
        // Research
        AgentRole {
            name: "researcher".into(), category: RoleCategory::Research,
            description: "Research topics, analyze codebases, gather information".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Reasoning,
            allowed_tools: vec!["Read".into(), "Glob".into(), "Grep".into(), "WebFetch".into(), "WebSearch".into(), "Bash".into()],
            denied_tools: vec!["Write".into(), "Edit".into()],
            system_prompt_extension: "You are a research analyst. Gather information, analyze codebases, compare approaches. Be thorough and cite sources.".into(),
        },
        // Project Management
        AgentRole {
            name: "project-planner".into(), category: RoleCategory::ProjectManagement,
            description: "Breaks down projects into tasks, estimates, and dependencies".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Reasoning,
            allowed_tools: vec!["Read".into(), "Glob".into(), "Grep".into(), "TaskCreate".into(), "TaskUpdate".into(), "TaskList".into()],
            denied_tools: vec!["Bash".into(), "Write".into(), "Edit".into()],
            system_prompt_extension: "You are a project planner. Break work into tasks with clear descriptions, estimate complexity, identify dependencies. Use TaskCreate to track work.".into(),
        },
        // Specialized
        AgentRole {
            name: "bug-hunter".into(), category: RoleCategory::CoreDevelopment,
            description: "Investigates and fixes bugs by analyzing code and logs".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Reasoning,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Edit".into(), "Glob".into(), "Grep".into()],
            denied_tools: vec![],
            system_prompt_extension: "You are a bug hunter. Investigate bugs systematically: reproduce, isolate, diagnose root cause, fix, verify. Check git blame for recent changes.".into(),
        },
        AgentRole {
            name: "refactoring-specialist".into(), category: RoleCategory::CoreDevelopment,
            description: "Refactors code for better structure, performance, and maintainability".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Coding,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Write".into(), "Edit".into(), "Glob".into(), "Grep".into()],
            denied_tools: vec![],
            system_prompt_extension: "You are a refactoring specialist. Improve code structure without changing behavior. Run tests before and after. Make small, verifiable changes.".into(),
        },
        AgentRole {
            name: "performance-optimizer".into(), category: RoleCategory::CoreDevelopment,
            description: "Optimizes code for speed, memory, and efficiency".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Reasoning,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Edit".into(), "Glob".into(), "Grep".into()],
            denied_tools: vec![],
            system_prompt_extension: "You are a performance optimizer. Profile, benchmark, identify bottlenecks, optimize. Focus on algorithmic improvements, not micro-optimizations.".into(),
        },
        AgentRole {
            name: "api-designer".into(), category: RoleCategory::Backend,
            description: "Designs and implements REST/GraphQL APIs".into(),
            min_model_params: 32_000_000_000, preferred_model_type: ModelPreference::Coding,
            allowed_tools: vec!["Bash".into(), "Read".into(), "Write".into(), "Edit".into(), "Glob".into(), "Grep".into(), "WebFetch".into()],
            denied_tools: vec![],
            system_prompt_extension: "You are an API designer. Design clean, RESTful APIs with proper status codes, error handling, and documentation. Consider versioning and backward compatibility.".into(),
        },
    ]
}

/// Find a role by name.
pub fn find_role(name: &str) -> Option<AgentRole> {
    builtin_roles().into_iter().find(|r| r.name == name)
}

/// List roles by category.
pub fn roles_by_category(category: RoleCategory) -> Vec<AgentRole> {
    builtin_roles().into_iter().filter(|r| r.category == category).collect()
}

/// List all role names.
pub fn role_names() -> Vec<String> {
    builtin_roles().into_iter().map(|r| r.name).collect()
}
