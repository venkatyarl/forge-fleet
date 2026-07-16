"""Project Guard — prevent files from being written to wrong locations.

Guardrails:
1. Each project defines allowed file paths
2. write_file rejects paths outside allowed directories
3. Pre-commit check verifies file locations
4. Branch naming follows project conventions
"""
import os
from dataclasses import dataclass, field


@dataclass
class ProjectConfig:
    """Configuration for a specific project's file structure."""
    name: str
    repo_dir: str
    
    # Allowed directories for code files
    allowed_paths: list = field(default_factory=list)
    
    # Explicitly blocked paths (catch common mistakes)
    blocked_paths: list = field(default_factory=list)
    
    # Branch naming prefix
    branch_prefix: str = "feat"
    
    # Tech stack
    backend: str = ""
    frontend: str = ""
    database: str = ""


# ─── Project Configs ────────────────────────────────────

HIREFLOW360 = ProjectConfig(
    name="HireFlow360",
    repo_dir="/Users/venkat/taylorProjects/HireFlow360",
    allowed_paths=[
        "rust-backend/crates/",       # All Rust code
        "frontend/src/",              # All React/TS code
        "rust-backend/migrations/",   # SQL migrations
        "docs/",                      # Documentation
        ".github/",                   # CI/CD
        "docker/",                    # Docker configs
    ],
    blocked_paths=[
        "src/",                       # Root src/ is WRONG
        "app.py",                     # Python files don't belong
        "requirements.txt",           # Python deps don't belong
        "models.py",                  # Python models don't belong
        "database.py",               # Python DB don't belong
        "templates/",                 # Jinja templates don't belong
    ],
    branch_prefix="feat/hf",
    backend="Rust + Axum",
    frontend="Next.js + React + TypeScript",
    database="PostgreSQL",
)

FORGEFLEET = ProjectConfig(
    name="ForgeFleet",
    repo_dir="/Users/venkat/taylorProjects/forge-fleet",
    allowed_paths=[
        "forgefleet/",               # All ForgeFleet code
    ],
    blocked_paths=[
        "rust-backend/",             # HireFlow code doesn't belong
        "frontend/",                 # HireFlow frontend doesn't belong
    ],
    branch_prefix="feat/ff",
    backend="Python",
)

PROJECTS = {
    "HireFlow360": HIREFLOW360,
    "ForgeFleet": FORGEFLEET,
}


class ProjectGuard:
    """Validates file operations against project rules."""
    
    def __init__(self, project: ProjectConfig):
        self.project = project
    
    def validate_write(self, filepath: str) -> tuple[bool, str]:
        """Check if a file path is allowed for this project.
        
        Returns (allowed, reason).
        """
        # Check blocked paths first
        for blocked in self.project.blocked_paths:
            if filepath.startswith(blocked) or filepath == blocked:
                return False, f"BLOCKED: '{filepath}' is not allowed in {self.project.name}. Use {self.project.allowed_paths[0]} instead."
        
        # Check if path is in allowed directories
        if self.project.allowed_paths:
            in_allowed = any(filepath.startswith(p) for p in self.project.allowed_paths)
            # Allow root-level config files
            root_allowed = filepath in ("Cargo.toml", "package.json", "docker-compose.yml", 
                                        "README.md", ".gitignore", "Cargo.lock")
            
            if not in_allowed and not root_allowed:
                return False, (
                    f"PATH ERROR: '{filepath}' is outside allowed directories.\n"
                    f"Allowed: {', '.join(self.project.allowed_paths)}\n"
                    f"Hint: For Rust code, use rust-backend/crates/CRATE_NAME/src/{os.path.basename(filepath)}"
                )
        
        return True, "OK"
    
    def validate_all_changes(self, changed_files: list[str]) -> list[str]:
        """Validate all changed files. Returns list of violations."""
        violations = []
        for filepath in changed_files:
            allowed, reason = self.validate_write(filepath)
            if not allowed:
                violations.append(reason)
        return violations
    
    def get_branch_name(self, ticket_id: str) -> str:
        """Generate a branch name following project conventions."""
        return f"{self.project.branch_prefix}-{ticket_id[:8]}"
    
    def get_tech_stack(self) -> dict:
        """Get project tech stack as dict for agent prompts."""
        return {
            "backend": self.project.backend,
            "frontend": self.project.frontend,
            "database": self.project.database,
            "structure": f"Files must go in: {', '.join(self.project.allowed_paths[:3])}",
            "blocked": f"NEVER create files in: {', '.join(self.project.blocked_paths[:3])}",
        }
    
    def wrap_write_file(self, original_func):
        """Wrap a write_file function with path validation."""
        guard = self
        
        def guarded_write(filepath="", content=""):
            allowed, reason = guard.validate_write(filepath)
            if not allowed:
                return f"REJECTED: {reason}"
            return original_func(filepath=filepath, content=content)
        
        return guarded_write


def get_project(repo_dir: str) -> ProjectConfig:
    """Detect which project a repo directory belongs to."""
    for name, config in PROJECTS.items():
        if config.repo_dir in repo_dir or repo_dir in config.repo_dir:
            return config
    
    # Unknown project — return permissive config
    return ProjectConfig(name="Unknown", repo_dir=repo_dir)
