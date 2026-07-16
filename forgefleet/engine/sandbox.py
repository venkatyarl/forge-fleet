"""Sandbox — run LLM-generated code in Docker before applying to real repo.

Like Codex: changes are tested in isolation. If they break, the real repo is untouched.

Flow:
1. Copy repo to a temp Docker volume
2. Apply LLM's edits inside the container
3. Run cargo check / npm build / pytest inside container
4. If it passes → apply to real repo
5. If it fails → return error to LLM for fixing (never touches real files)
"""
import json
import os
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class SandboxResult:
    """Result of running code in a sandbox."""
    success: bool
    exit_code: int = 0
    stdout: str = ""
    stderr: str = ""
    duration: float = 0
    files_changed: list = field(default_factory=list)


class DockerSandbox:
    """Run code changes in an isolated Docker container.
    
    Uses a lightweight container with the project's build tools.
    Changes are tested before being applied to the real repo.
    """
    
    # Base images for different project types
    IMAGES = {
        "rust": "rust:1.82-slim",
        "node": "node:22-slim",
        "python": "python:3.12-slim",
    }
    
    def __init__(self, repo_dir: str, project_type: str = ""):
        self.repo_dir = repo_dir
        self.project_type = project_type or self._detect_project()
        self.container_name = f"forgefleet-sandbox-{os.getpid()}"
    
    def _detect_project(self) -> str:
        """Detect project type from files present."""
        if os.path.exists(os.path.join(self.repo_dir, "Cargo.toml")):
            return "rust"
        if os.path.exists(os.path.join(self.repo_dir, "package.json")):
            return "node"
        if os.path.exists(os.path.join(self.repo_dir, "pyproject.toml")):
            return "python"
        return "rust"  # Default if no package.json or pyproject.toml
    
    def test_changes(self, changed_files: dict, check_command: str = "") -> SandboxResult:
        """Test file changes in a Docker sandbox.
        
        Args:
            changed_files: dict of {filepath: new_content}
            check_command: command to verify (default: auto-detect)
            
        Returns:
            SandboxResult with pass/fail and output
        """
        if not check_command:
            check_command = self._default_check_command()
        
        start = time.time()
        
        # Create a temp directory with the changes
        with tempfile.TemporaryDirectory(prefix="forgefleet-") as tmpdir:
            # Write changed files to temp dir
            for filepath, content in changed_files.items():
                full_path = os.path.join(tmpdir, filepath)
                os.makedirs(os.path.dirname(full_path), exist_ok=True)
                Path(full_path).write_text(content)
            
            # Build Docker command
            image = self.IMAGES.get(self.project_type, "rust:1.82-slim")
            
            # Mount real repo as read-only base, overlay changes on top
            docker_cmd = [
                "docker", "run", "--rm",
                "--name", self.container_name,
                "--network", "none",  # No network access for safety
                "--memory", "4g",     # Memory limit
                "--cpus", "2",        # CPU limit
                "-v", f"{self.repo_dir}:/repo:ro",      # Real repo (read-only)
                "-v", f"{tmpdir}:/changes:ro",            # Changed files
                image,
                "bash", "-c", self._build_sandbox_script(changed_files.keys(), check_command),
            ]
            
            try:
                r = subprocess.run(
                    docker_cmd, capture_output=True, text=True,
                    timeout=300,  # 5 min max
                )
                
                duration = time.time() - start
                stdout = r.stdout[-5000:] if len(r.stdout) > 5000 else r.stdout
                stderr = r.stderr[-5000:] if len(r.stderr) > 5000 else r.stderr
                
                return SandboxResult(
                    success=r.returncode == 0,
                    exit_code=r.returncode,
                    stdout=stdout,
                    stderr=stderr,
                    duration=round(duration, 1),
                    files_changed=list(changed_files.keys()),
                )
                
            except subprocess.TimeoutExpired:
                self._kill_container()
                return SandboxResult(
                    success=False, exit_code=-1,
                    stderr="Sandbox timed out after 300s",
                    duration=300,
                )
            except Exception as e:
                return SandboxResult(
                    success=False, exit_code=-1,
                    stderr=f"Sandbox error: {e}",
                )
    
    def _build_sandbox_script(self, changed_files, check_command: str) -> str:
        """Build the script that runs inside the container."""
        # Copy repo, overlay changes, run check
        copy_changes = ""
        for filepath in changed_files:
            dirname = os.path.dirname(filepath)
            if dirname:
                copy_changes += f"mkdir -p /work/{dirname} && "
            copy_changes += f"cp /changes/{filepath} /work/{filepath} && "
        
        return f"""
            set -e
            # Copy repo to writable location
            cp -r /repo /work
            cd /work
            # Apply changes
            {copy_changes}true
            # Run verification
            {check_command}
        """
    
    def _default_check_command(self) -> str:
        """Default check command based on project type."""
        if self.project_type == "rust":
            return "cargo check 2>&1"
        elif self.project_type == "node":
            return "npm install --ignore-scripts 2>&1 && npm run build 2>&1"
        elif self.project_type == "python":
            return "python -m py_compile *.py 2>&1"
        return "echo 'No check command'"
    
    def _kill_container(self):
        """Kill a running sandbox container."""
        try:
            subprocess.run(
                ["docker", "kill", self.container_name],
                capture_output=True, timeout=10,
            )
        except Exception:
            pass
    
    def quick_check(self, filepath: str, content: str) -> SandboxResult:
        """Quick check a single file change."""
        return self.test_changes({filepath: content})
    
    @staticmethod
    def is_available() -> bool:
        """Check if Docker is available for sandboxing."""
        try:
            r = subprocess.run(
                ["docker", "info"], capture_output=True, timeout=5,
            )
            return r.returncode == 0
        except Exception:
            return False
