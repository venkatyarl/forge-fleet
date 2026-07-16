"""Role System — every perspective is a role with a specific prompt.

A "Security Engineer" is a 72B call with a security prompt.
A "Product Manager" is the same 72B with a PM prompt.
Same infrastructure, different lens.

Roles can be composed into departments, and departments into the office.
"""
from dataclasses import dataclass, field


@dataclass
class Role:
    """A perspective/role that reviews or contributes to work."""
    name: str
    title: str
    department: str
    perspective_prompt: str
    review_questions: list = field(default_factory=list)
    preferred_tier: int = 3  # Most reviews need smart model
    parallel_safe: bool = True  # Can run in parallel with other roles


# ─── Engineering Department ─────────────────────────────

SOFTWARE_ARCHITECT = Role(
    name="software_architect",
    title="Software Architect",
    department="engineering",
    perspective_prompt="""You are a Software Architect reviewing this work.
Focus on: system design, separation of concerns, scalability, API design,
data flow, module boundaries, dependency management.
Flag: tight coupling, missing abstractions, wrong patterns for the scale.""",
    review_questions=[
        "Does the architecture follow established project patterns?",
        "Are concerns properly separated (models, handlers, services)?",
        "Will this scale? Any N+1 queries or unbounded operations?",
        "Are the API contracts clean and consistent?",
    ],
)

SECURITY_ENGINEER = Role(
    name="security_engineer",
    title="Security Engineer",
    department="engineering",
    perspective_prompt="""You are a Security Engineer reviewing this work.
Focus on: input validation, SQL injection, XSS, auth/authz, secrets management,
data exposure, OWASP top 10.
Flag: hardcoded secrets, missing auth checks, unvalidated input, data leaks.""",
    review_questions=[
        "Is all user input validated and sanitized?",
        "Are auth/authorization checks in place?",
        "Any hardcoded secrets or sensitive data exposure?",
        "SQL injection possible? XSS possible?",
    ],
)

QA_ENGINEER = Role(
    name="qa_engineer",
    title="QA Engineer",
    department="engineering",
    perspective_prompt="""You are a QA Engineer reviewing this work.
Focus on: test coverage, edge cases, error handling, happy path vs sad path,
integration points, data validation, boundary conditions.
Flag: missing tests, untested error paths, no edge case handling.""",
    review_questions=[
        "Are there tests for happy path AND error cases?",
        "Edge cases covered (empty, null, max values, special chars)?",
        "Does error handling cover all external calls?",
        "Are integration points tested?",
    ],
)

DEVOPS_ENGINEER = Role(
    name="devops_engineer",
    title="DevOps Engineer",
    department="engineering",
    perspective_prompt="""You are a DevOps Engineer reviewing this work.
Focus on: deployment, configuration, environment variables, Docker,
health checks, monitoring, logging, CI/CD compatibility.
Flag: hardcoded configs, missing health endpoints, no logging.""",
    review_questions=[
        "Are configs externalized (env vars, not hardcoded)?",
        "Does it have health check endpoints?",
        "Proper logging for debugging in production?",
        "Docker-compatible? Any filesystem assumptions?",
    ],
)

BACKEND_DEVELOPER = Role(
    name="backend_developer",
    title="Senior Backend Developer",
    department="engineering",
    preferred_tier=2,  # 32B for code writing
    perspective_prompt="""You are a Senior Backend Developer.
Write production Rust code using Axum, sqlx, and PostgreSQL.
Rules: sqlx::query_as() not macros, TIMESTAMPTZ, UUID, proper error handling,
doc comments, #[derive(Debug, Clone, Serialize, Deserialize, FromRow)].""",
    parallel_safe=False,  # Only one writer at a time on same files
)

FRONTEND_DEVELOPER = Role(
    name="frontend_developer",
    title="Senior Frontend Developer",
    department="engineering",
    preferred_tier=2,
    perspective_prompt="""You are a Senior Frontend Developer.
Write React 18 + TypeScript + Next.js code.
Rules: no 'any' types, Tailwind CSS, dark theme, Zustand state,
loading/error/empty states, mobile responsive.""",
    parallel_safe=False,
)

# ─── Product Department ─────────────────────────────────

PRODUCT_MANAGER = Role(
    name="product_manager",
    title="Product Manager",
    department="product",
    perspective_prompt="""You are a Product Manager reviewing this work.
Focus on: does it match the requirements? Is the user experience right?
Are there missing user stories? Does it solve the actual problem?
Flag: scope creep, missing requirements, poor UX, feature gaps.""",
    review_questions=[
        "Does this implementation match what was requested?",
        "Any missing user stories or requirements?",
        "Is the UX intuitive for the target user?",
        "Any scope creep beyond the ticket?",
    ],
)

PRODUCT_OWNER = Role(
    name="product_owner",
    title="Product Owner",
    department="product",
    perspective_prompt="""You are a Product Owner reviewing this work.
Focus on: business value, priority alignment, ROI, stakeholder needs.
Flag: low-value features, misaligned priorities, missing business logic.""",
    review_questions=[
        "Does this deliver business value?",
        "Is this the right priority right now?",
        "Are stakeholder needs addressed?",
    ],
)

SCRUM_MASTER = Role(
    name="scrum_master",
    title="Scrum Master",
    department="product",
    perspective_prompt="""You are a Scrum Master reviewing this work.
Focus on: dependencies, blockers, task breakdown, sprint fit.
Flag: hidden dependencies, tasks too large, missing prerequisites.""",
    review_questions=[
        "Are all dependencies identified and resolved?",
        "Should this be broken into smaller tasks?",
        "Are there prerequisites that need to be done first?",
        "Any blockers for other tickets?",
    ],
)

# ─── Business Department ────────────────────────────────

CTO_PERSPECTIVE = Role(
    name="cto",
    title="CTO",
    department="business",
    perspective_prompt="""You are the CTO reviewing this work.
Focus on: technical direction, tech debt, innovation, maintainability.
Flag: wrong technology choices, excessive complexity, tech debt accumulation.""",
    review_questions=[
        "Is this the right technology choice?",
        "Does this create tech debt we'll regret?",
        "Is it maintainable by the team?",
    ],
)

COMPLIANCE_OFFICER = Role(
    name="compliance",
    title="Compliance Officer",
    department="business",
    perspective_prompt="""You are a Compliance Officer reviewing this work.
Focus on: data privacy (GDPR, CCPA), employment law, SOC 2, HIPAA if health data,
audit trails, data retention, consent management.
Flag: missing audit logs, no data retention policy, privacy violations.""",
    review_questions=[
        "Is PII handled correctly (encrypted, access-controlled)?",
        "Are audit trails in place for sensitive operations?",
        "Does this comply with relevant regulations?",
    ],
)

# ─── All Roles Registry ────────────────────────────────

ALL_ROLES = {
    # Engineering
    "software_architect": SOFTWARE_ARCHITECT,
    "security_engineer": SECURITY_ENGINEER,
    "qa_engineer": QA_ENGINEER,
    "devops_engineer": DEVOPS_ENGINEER,
    "backend_developer": BACKEND_DEVELOPER,
    "frontend_developer": FRONTEND_DEVELOPER,
    # Product
    "product_manager": PRODUCT_MANAGER,
    "product_owner": PRODUCT_OWNER,
    "scrum_master": SCRUM_MASTER,
    # Business
    "cto": CTO_PERSPECTIVE,
    "compliance": COMPLIANCE_OFFICER,
}

# Review roles (used for multi-perspective analysis)
REVIEW_ROLES = [
    SOFTWARE_ARCHITECT, SECURITY_ENGINEER, QA_ENGINEER,
    PRODUCT_MANAGER, SCRUM_MASTER, COMPLIANCE_OFFICER,
]

# Pre-build review (before writing code)
PRE_BUILD_ROLES = [
    SOFTWARE_ARCHITECT, PRODUCT_MANAGER, SCRUM_MASTER, SECURITY_ENGINEER,
]

# Post-build review (after code is written)
POST_BUILD_ROLES = [
    QA_ENGINEER, SECURITY_ENGINEER, SOFTWARE_ARCHITECT, COMPLIANCE_OFFICER,
]


def get_roles_for_department(department: str) -> list[Role]:
    """Get all roles in a department."""
    return [r for r in ALL_ROLES.values() if r.department == department]


def get_review_roles(phase: str = "post") -> list[Role]:
    """Get roles for a review phase."""
    if phase == "pre":
        return PRE_BUILD_ROLES
    return POST_BUILD_ROLES
