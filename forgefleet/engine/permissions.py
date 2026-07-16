"""Permission System — guardrails for autonomous agents.

Like Claude Code: agents must ask before dangerous operations.
Prevents accidental file deletion, destructive git operations,
or running harmful commands.
"""
import os
import re
from dataclasses import dataclass, field
from enum import Enum


class Action(Enum):
    READ_FILE = "read_file"
    WRITE_FILE = "write_file"
    DELETE_FILE = "delete_file"
    CREATE_DIR = "create_dir"
    RUN_COMMAND = "run_command"
    GIT_PUSH = "git_push"
    GIT_FORCE_PUSH = "git_force_push"
    GIT_RESET = "git_reset"
    NETWORK_REQUEST = "network_request"
    INSTALL_PACKAGE = "install_package"


class Decision(Enum):
    ALLOW = "allow"
    DENY = "deny"
    ASK = "ask"  # Ask the user (via OpenClaw bridge)


# Commands that are always dangerous
DANGEROUS_COMMANDS = {
    r"rm\s+-rf": "Recursive force delete",
    r"rm\s+-r\s+/": "Delete from root",
    r"git\s+reset\s+--hard": "Hard reset (loses changes)",
    r"git\s+push\s+--force\b": "Force push (overwrites history)",
    r"git\s+push\s+-f\b": "Force push (overwrites history)",
    r"sudo\s+rm": "Sudo delete",
    r"mkfs": "Format filesystem",
    r"dd\s+if=": "Raw disk write",
    r">\s*/dev/": "Write to device",
    r"chmod\s+-R\s+777": "World-writable permissions",
    r"curl.*\|\s*(?:bash|sh)": "Pipe URL to shell",
    r"wget.*\|\s*(?:bash|sh)": "Pipe URL to shell",
}

# File paths that should never be modified
PROTECTED_PATHS = {
    ".git/",
    ".env",
    ".ssh/",
    "~/.openclaw/openclaw.json",
    "/etc/",
    "/usr/",
    "/System/",
}

# Safe commands that are always allowed
SAFE_COMMANDS = {
    r"^cargo\s+(check|build|test|clippy|fmt)",
    r"^npm\s+(run|test|build|install)",
    r"^python3?\s+-m\s+(pytest|py_compile)",
    r"^git\s+(status|log|diff|branch|show)",
    r"^ls\b",
    r"^cat\b",
    r"^head\b",
    r"^tail\b",
    r"^grep\b",
    r"^find\b",
    r"^wc\b",
}


@dataclass
class PermissionCheck:
    """Result of a permission check."""
    action: Action
    target: str
    decision: Decision
    reason: str = ""


class PermissionGuard:
    """Checks agent actions against safety rules.
    
    Default policy: allow safe operations, deny dangerous ones,
    ask for anything ambiguous.
    """
    
    def __init__(self, repo_dir: str, auto_approve_safe: bool = True):
        self.repo_dir = repo_dir
        self.auto_approve_safe = auto_approve_safe
        self.approved_patterns: set = set()  # User-approved patterns
        self.denied_patterns: set = set()
    
    def check(self, action: Action, target: str) -> PermissionCheck:
        """Check if an action is allowed."""
        
        if action == Action.READ_FILE:
            return PermissionCheck(action, target, Decision.ALLOW, "Read is always safe")
        
        if action == Action.WRITE_FILE:
            return self._check_write(target)
        
        if action == Action.DELETE_FILE:
            return self._check_delete(target)
        
        if action == Action.RUN_COMMAND:
            return self._check_command(target)
        
        if action == Action.GIT_PUSH:
            return PermissionCheck(action, target, Decision.ALLOW, "Push to feature branch")
        
        if action == Action.GIT_FORCE_PUSH:
            return PermissionCheck(action, target, Decision.DENY, "Force push not allowed in autonomous mode")
        
        if action == Action.GIT_RESET:
            return PermissionCheck(action, target, Decision.DENY, "Hard reset not allowed in autonomous mode")
        
        return PermissionCheck(action, target, Decision.ASK, "Unknown action type")
    
    def _check_write(self, filepath: str) -> PermissionCheck:
        """Check if writing to a file is safe."""
        # Check protected paths
        for protected in PROTECTED_PATHS:
            if filepath.startswith(protected) or protected in filepath:
                return PermissionCheck(
                    Action.WRITE_FILE, filepath, Decision.DENY,
                    f"Protected path: {protected}"
                )
        
        # Must be within repo
        abs_path = os.path.abspath(os.path.join(self.repo_dir, filepath))
        if not abs_path.startswith(os.path.abspath(self.repo_dir)):
            return PermissionCheck(
                Action.WRITE_FILE, filepath, Decision.DENY,
                "Path escapes repository directory"
            )
        
        return PermissionCheck(Action.WRITE_FILE, filepath, Decision.ALLOW, "Within repo boundary")
    
    def _check_delete(self, filepath: str) -> PermissionCheck:
        """Check if deleting a file is safe."""
        # Never allow deleting outside repo
        abs_path = os.path.abspath(os.path.join(self.repo_dir, filepath))
        if not abs_path.startswith(os.path.abspath(self.repo_dir)):
            return PermissionCheck(Action.DELETE_FILE, filepath, Decision.DENY, "Outside repo")
        
        # Ask for any delete
        return PermissionCheck(
            Action.DELETE_FILE, filepath, Decision.ASK,
            f"Agent wants to delete: {filepath}"
        )
    
    def _check_command(self, command: str) -> PermissionCheck:
        """Check if a shell command is safe to run."""
        # Check against dangerous patterns
        for pattern, reason in DANGEROUS_COMMANDS.items():
            if re.search(pattern, command):
                return PermissionCheck(
                    Action.RUN_COMMAND, command, Decision.DENY,
                    f"Dangerous: {reason}"
                )
        
        # Check against safe patterns
        if self.auto_approve_safe:
            for pattern in SAFE_COMMANDS:
                if re.match(pattern, command.strip()):
                    return PermissionCheck(
                        Action.RUN_COMMAND, command, Decision.ALLOW,
                        "Known safe command"
                    )
        
        # Check previously approved
        if command in self.approved_patterns:
            return PermissionCheck(Action.RUN_COMMAND, command, Decision.ALLOW, "Previously approved")
        
        # Unknown command — ask
        return PermissionCheck(
            Action.RUN_COMMAND, command, Decision.ASK,
            f"Unknown command: {command[:60]}"
        )
    
    def approve(self, pattern: str):
        """Approve a command pattern for future use."""
        self.approved_patterns.add(pattern)
    
    def deny(self, pattern: str):
        """Deny a command pattern."""
        self.denied_patterns.add(pattern)
