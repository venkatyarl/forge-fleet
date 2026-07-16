"""Git Operations — branch, commit, push, PR creation.

Extracted from mini-SWE-agent's edit-verify-commit cycle (MIT license).
Handles the full git workflow for autonomous coding agents.
"""
import os
import subprocess
import time
from dataclasses import dataclass, field


@dataclass
class GitResult:
    """Result of a git operation."""
    success: bool
    command: str
    output: str = ""
    error: str = ""


class GitOps:
    """Git operations for autonomous coding agents.
    
    Workflow:
    1. Create branch from main
    2. Make changes (via DiffEditor)
    3. Stage + commit
    4. Push to remote
    5. Create PR (via gh CLI)
    """
    
    def __init__(self, repo_dir: str):
        self.repo_dir = repo_dir
    
    def _run(self, *args, timeout: int = 30) -> GitResult:
        """Run a git command."""
        cmd = ["git"] + list(args)
        try:
            r = subprocess.run(
                cmd, capture_output=True, text=True,
                timeout=timeout, cwd=self.repo_dir,
            )
            return GitResult(
                success=r.returncode == 0,
                command=" ".join(cmd),
                output=r.stdout.strip(),
                error=r.stderr.strip(),
            )
        except subprocess.TimeoutExpired:
            return GitResult(success=False, command=" ".join(cmd), error="Timeout")
        except Exception as e:
            return GitResult(success=False, command=" ".join(cmd), error=str(e))
    
    def current_branch(self) -> str:
        """Get current branch name."""
        r = self._run("branch", "--show-current")
        return r.output if r.success else "unknown"
    
    def create_branch(self, branch_name: str, from_branch: str = "main") -> GitResult:
        """Create and checkout a new branch."""
        # Fetch latest
        self._run("fetch", "origin", from_branch)
        
        # Create branch from latest remote
        result = self._run("checkout", "-b", branch_name, f"origin/{from_branch}")
        if not result.success and "already exists" in result.error:
            result = self._run("checkout", branch_name)
        
        return result
    
    def stage_all(self) -> GitResult:
        """Stage all changes."""
        return self._run("add", "-A")
    
    def stage_files(self, files: list[str]) -> GitResult:
        """Stage specific files."""
        return self._run("add", *files)
    
    def commit(self, message: str) -> GitResult:
        """Commit staged changes."""
        return self._run("commit", "-m", message)
    
    def push(self, branch_name: str = "", force: bool = False) -> GitResult:
        """Push to remote."""
        if not branch_name:
            branch_name = self.current_branch()
        
        args = ["push", "origin", branch_name]
        if force:
            args.insert(1, "--force-with-lease")
        
        return self._run(*args, timeout=60)
    
    def diff_stat(self) -> str:
        """Get a summary of uncommitted changes."""
        r = self._run("diff", "--stat")
        staged = self._run("diff", "--staged", "--stat")
        
        parts = []
        if r.output:
            parts.append(f"Unstaged:\n{r.output}")
        if staged.output:
            parts.append(f"Staged:\n{staged.output}")
        return "\n".join(parts) if parts else "No changes"
    
    def has_changes(self) -> bool:
        """Check if there are uncommitted changes."""
        r = self._run("status", "--porcelain")
        return bool(r.output.strip())
    
    def stash(self, message: str = "auto-stash") -> GitResult:
        """Stash current changes."""
        return self._run("stash", "push", "-m", message)
    
    def stash_pop(self) -> GitResult:
        """Pop the latest stash."""
        return self._run("stash", "pop")
    
    def create_pr(self, title: str, body: str = "", base: str = "main",
                  draft: bool = False) -> GitResult:
        """Create a PR using gh CLI."""
        branch = self.current_branch()
        
        cmd = ["gh", "pr", "create",
               "--title", title,
               "--body", body or f"Automated PR from ForgeFleet\n\nBranch: {branch}",
               "--base", base,
               "--head", branch]
        
        if draft:
            cmd.append("--draft")
        
        try:
            r = subprocess.run(
                cmd, capture_output=True, text=True,
                timeout=30, cwd=self.repo_dir,
            )
            return GitResult(
                success=r.returncode == 0,
                command=" ".join(cmd),
                output=r.stdout.strip(),
                error=r.stderr.strip(),
            )
        except Exception as e:
            return GitResult(success=False, command="gh pr create", error=str(e))
    
    def full_cycle(self, branch_name: str, commit_message: str,
                   pr_title: str = "", files: list[str] = None) -> dict:
        """Full git cycle: branch → stage → commit → push → PR.
        
        Returns dict with results of each step.
        """
        results = {}
        
        # 1. Create branch
        results["branch"] = self.create_branch(branch_name)
        if not results["branch"].success:
            return results
        
        # 2. Stage
        if files:
            results["stage"] = self.stage_files(files)
        else:
            results["stage"] = self.stage_all()
        
        if not results["stage"].success:
            return results
        
        # 3. Commit
        results["commit"] = self.commit(commit_message)
        if not results["commit"].success:
            return results
        
        # 4. Push
        results["push"] = self.push(branch_name)
        if not results["push"].success:
            return results
        
        # 5. PR (optional)
        if pr_title:
            results["pr"] = self.create_pr(pr_title)
        
        results["success"] = True
        return results
