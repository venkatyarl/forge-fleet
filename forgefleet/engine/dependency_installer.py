"""Dependency Installer — cargo add / npm install as agent tool.

Last Codex gap: agents can add new dependencies when needed.
"""
import os
import subprocess
from dataclasses import dataclass
from .tool import Tool


class DependencyInstaller:
    """Install project dependencies as part of the agent workflow."""
    
    def __init__(self, repo_dir: str):
        self.repo_dir = repo_dir
    
    def cargo_add(self, crate: str, features: str = "") -> str:
        """Add a Rust crate dependency."""
        cmd = ["cargo", "add", crate]
        if features:
            cmd.extend(["--features", features])
        
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=60, cwd=self.repo_dir)
            if r.returncode == 0:
                return f"Added crate: {crate}" + (f" with features: {features}" if features else "")
            return f"Failed to add {crate}: {r.stderr[:200]}"
        except Exception as e:
            return f"Error: {e}"
    
    def npm_install(self, package: str, dev: bool = False) -> str:
        """Add an npm package dependency."""
        cmd = ["npm", "install", package]
        if dev:
            cmd.append("--save-dev")
        
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=120, cwd=self.repo_dir)
            if r.returncode == 0:
                return f"Installed: {package}" + (" (dev)" if dev else "")
            return f"Failed: {r.stderr[:200]}"
        except Exception as e:
            return f"Error: {e}"
    
    def pip_install(self, package: str) -> str:
        """Add a Python package."""
        try:
            r = subprocess.run(
                ["pip3", "install", package],
                capture_output=True, text=True, timeout=120, cwd=self.repo_dir,
            )
            if r.returncode == 0:
                return f"Installed: {package}"
            return f"Failed: {r.stderr[:200]}"
        except Exception as e:
            return f"Error: {e}"
    
    def as_tools(self) -> list[Tool]:
        """Return Tool objects for agent use."""
        return [
            Tool(
                name="cargo_add",
                description="Add a Rust crate dependency to Cargo.toml",
                parameters={"type": "object", "properties": {
                    "crate": {"type": "string", "description": "Crate name"},
                    "features": {"type": "string", "description": "Comma-separated features"},
                }, "required": ["crate"]},
                func=lambda crate="", features="": self.cargo_add(crate, features),
            ),
            Tool(
                name="npm_install",
                description="Add an npm package to package.json",
                parameters={"type": "object", "properties": {
                    "package": {"type": "string", "description": "Package name"},
                    "dev": {"type": "boolean", "description": "Install as dev dependency"},
                }, "required": ["package"]},
                func=lambda package="", dev=False: self.npm_install(package, dev),
            ),
        ]
