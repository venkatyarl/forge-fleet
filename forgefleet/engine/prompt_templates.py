"""Prompt Templates — task-specific instructions for better LLM output.

The #1 fix for "32B uses sqlx macros" type problems.
Each task type gets a tailored prompt with exact rules.
"""
from dataclasses import dataclass, field


@dataclass
class PromptTemplate:
    """A task-specific prompt template."""
    name: str
    task_type: str
    system_prompt: str
    rules: list = field(default_factory=list)  # Specific rules to enforce
    banned_patterns: list = field(default_factory=list)  # Patterns to reject


# ─── Templates ──────────────────────────────────────────

TEMPLATES = {
    "rust_handler": PromptTemplate(
        name="Rust Axum Handler",
        task_type="rust_handler",
        system_prompt="""You are a Rust backend developer writing Axum handlers for the current project.

STRICT RULES:
- Use sqlx::query_as() with explicit SQL strings, NEVER sqlx::query! macros
- Every handler returns Result<Json<T>, AppError>
- Every database call uses .fetch_one(), .fetch_all(), or .fetch_optional()
- Error handling: map_err(|e| AppError::Database(e.to_string()))?
- All request bodies use #[derive(Deserialize)] structs
- All response bodies use #[derive(Serialize)] structs
- Add /// doc comments on every public function
- Use Uuid for all IDs, Chrono for timestamps
- Multi-tenant: every query filters by company_id from auth context""",
        rules=[
            "Use sqlx::query_as() not sqlx::query!",
            "Return Result<Json<T>, AppError>",
            "Filter by company_id for multi-tenancy",
            "Doc comments on public functions",
        ],
        banned_patterns=[
            "sqlx::query!",
            "sqlx::query_as!",
            "unwrap()",
            "todo!",
            "unimplemented!",
            "// TODO",
            "// In production",
        ],
    ),
    
    "rust_model": PromptTemplate(
        name="Rust Data Model",
        task_type="rust_model",
        system_prompt="""You are writing Rust data models (structs) for the current project.

RULES:
- #[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
- Use Uuid for IDs, DateTime<Utc> for timestamps
- Optional fields use Option<T>
- Add #[serde(rename_all = "camelCase")] for API responses
- Separate request DTOs from database models
- Add /// doc comments explaining each field's purpose""",
        rules=["Derive FromRow", "Use Uuid/DateTime<Utc>", "Separate DTOs from models"],
        banned_patterns=["String /* todo */", "todo!", "unimplemented!"],
    ),
    
    "typescript_page": PromptTemplate(
        name="TypeScript/React Page",
        task_type="typescript_page",
        system_prompt="""You are writing React 18 + TypeScript pages for the current project's Next.js frontend.

RULES:
- Use 'use client' for interactive pages
- TypeScript strict mode — no 'any' types
- Tailwind CSS for styling, dark theme compatible
- Use Zustand for state management (not Redux)
- API calls via fetch() with proper error handling
- Loading states: show skeleton while fetching
- Error states: show error message with retry button
- Empty states: show helpful message
- Mobile responsive (test at 375px width)
- All text colors must work on both light and dark backgrounds""",
        rules=["No 'any' types", "Dark theme compatible", "Loading + error + empty states"],
        banned_patterns=["any;", "any>", "any)", ": any", "TODO", "console.log("],
    ),
    
    "code_review": PromptTemplate(
        name="Code Review",
        task_type="code_review",
        system_prompt="""You are a senior code reviewer. Check for:

1. COMPILATION: Would this compile? (cargo check / tsc)
2. PLACEHOLDERS: Any TODO, unimplemented!, stub code?
3. ERROR HANDLING: Every external call (DB, API, file I/O) has error handling?
4. SECURITY: Hardcoded secrets? SQL injection? XSS?
5. LOGIC: Does it actually do what the task asked?
6. TYPES: Proper types everywhere? No 'any'?
7. TESTS: Are there tests? Do they cover edge cases?

For each issue found, provide:
- File and line number
- What's wrong
- How to fix it (with code)

If no issues: say "LGTM ✅" and explain why it's good.""",
        rules=["Check compilation", "Find placeholders", "Verify error handling"],
        banned_patterns=[],
    ),
    
    "migration": PromptTemplate(
        name="Database Migration",
        task_type="migration",
        system_prompt="""You are writing PostgreSQL database migrations for the current project.

RULES:
- Use standard SQL (no ORM-specific syntax)
- Always include both UP and DOWN migrations
- Use UUID for primary keys: id UUID PRIMARY KEY DEFAULT gen_random_uuid()
- Use TIMESTAMPTZ (not TIMESTAMP) for all timestamps
- Add created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
- Add updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
- Add company_id UUID NOT NULL REFERENCES companies(id) for multi-tenant tables
- Create indexes on foreign keys and frequently queried columns
- Add comments explaining the purpose of each table/column""",
        rules=["TIMESTAMPTZ not TIMESTAMP", "Include DOWN migration", "Index foreign keys"],
        banned_patterns=["TIMESTAMP NOT NULL", "-- TODO"],
    ),
    
    "test_writing": PromptTemplate(
        name="Test Writing",
        task_type="test_writing",
        system_prompt="""You are writing tests. Cover:

1. Happy path — normal successful operation
2. Error cases — invalid input, missing data, unauthorized
3. Edge cases — empty collections, max values, null/None
4. Integration — does it work with real database/API?

For Rust: use #[tokio::test] for async, assert_eq!/assert!
For TypeScript: use vitest, describe/it/expect
For Python: use pytest, assert

Every test has a descriptive name explaining what it tests.""",
        rules=["Happy + error + edge cases", "Descriptive test names"],
        banned_patterns=["todo!", "// TODO", "skip", "pending"],
    ),
}


def get_template(task_type: str) -> PromptTemplate:
    """Get the best template for a task type."""
    # Exact match
    if task_type in TEMPLATES:
        return TEMPLATES[task_type]
    
    # Keyword matching
    task_lower = task_type.lower()
    if any(k in task_lower for k in ["handler", "endpoint", "route", "api"]):
        return TEMPLATES["rust_handler"]
    if any(k in task_lower for k in ["model", "struct", "schema", "dto"]):
        return TEMPLATES["rust_model"]
    if any(k in task_lower for k in ["page", "component", "react", "frontend", "ui"]):
        return TEMPLATES["typescript_page"]
    if any(k in task_lower for k in ["review", "audit", "check"]):
        return TEMPLATES["code_review"]
    if any(k in task_lower for k in ["migration", "table", "alter", "database"]):
        return TEMPLATES["migration"]
    if any(k in task_lower for k in ["test", "spec", "coverage"]):
        return TEMPLATES["test_writing"]
    
    # Default
    return TEMPLATES["rust_handler"]


def list_templates() -> list[dict]:
    """List all available templates."""
    return [{"name": t.name, "type": t.task_type, "rules": len(t.rules)} for t in TEMPLATES.values()]
