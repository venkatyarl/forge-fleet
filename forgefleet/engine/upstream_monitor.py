"""Upstream Monitor — track source projects for new features to absorb.

ForgeFleet is built from patterns extracted from these open-source projects:
- CrewAI (agent roles, task chaining, crew orchestration)
- Context Mode (FTS5 chunking, BM25 search, smart truncation)
- Strands (tool-calling loop format)
- Aider (unified diff parser, repo-map generation)
- CocoIndex (AST-based code search)
- Crush (code review patterns)
- mini-SWE-agent (edit-verify-commit cycle)
- AutoGen (multi-agent conversation patterns)
- LangGraph (durable execution, state machines)

This module:
1. Tracks the latest release/commit of each upstream project
2. Diffs against our last-checked version
3. Identifies new features worth absorbing
4. Creates tickets/notes for integration work
"""
import json
import os
import time
import urllib.request
import urllib.error
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class UpstreamProject:
    """An upstream project we extract patterns from."""
    name: str
    repo: str  # "owner/repo" format
    license: str
    what_we_took: str  # What patterns we extracted
    our_module: str  # Where it lives in ForgeFleet
    last_checked_sha: str = ""
    last_checked_tag: str = ""
    last_check_time: float = 0


# ─── Registry of upstream projects ─────────────────────

UPSTREAM_PROJECTS = [
    UpstreamProject(
        name="CrewAI",
        repo="crewAIInc/crewAI",
        license="MIT",
        what_we_took="Agent roles, task chaining, crew orchestration, sequential/hierarchical process",
        our_module="engine/agent.py, engine/task.py, engine/crew.py",
    ),
    UpstreamProject(
        name="Context Mode",
        repo="mksglu/context-mode",
        license="MIT",
        what_we_took="FTS5 chunking, BM25 search, smart head/tail truncation, markdown splitting",
        our_module="engine/context_store.py",
    ),
    UpstreamProject(
        name="Strands",
        repo="strands-agents/sdk-python",
        license="Apache-2.0",
        what_we_took="Tool-calling loop, OpenAI function calling format, manual fallback",
        our_module="engine/agent.py (tool-calling loop)",
    ),
    UpstreamProject(
        name="Aider",
        repo="paul-gauthier/aider",
        license="Apache-2.0",
        what_we_took="Unified diff parser, repo-map generation (planned)",
        our_module="engine/diff_editor.py (planned), engine/repo_map.py (planned)",
    ),
    UpstreamProject(
        name="CocoIndex",
        repo="cocoindex/cocoindex",
        license="Apache-2.0",
        what_we_took="AST-based code search (planned)",
        our_module="engine/context_store.py (search component)",
    ),
    UpstreamProject(
        name="mini-SWE-agent",
        repo="princeton-nlp/SWE-agent",
        license="MIT",
        what_we_took="Edit-verify-commit cycle (planned)",
        our_module="engine/git_ops.py (planned)",
    ),
    UpstreamProject(
        name="AutoGen",
        repo="microsoft/autogen",
        license="MIT",
        what_we_took="Multi-agent conversation patterns (planned)",
        our_module="(future: debate/review patterns)",
    ),
    UpstreamProject(
        name="LangGraph",
        repo="langchain-ai/langgraph",
        license="MIT",
        what_we_took="Durable execution concepts (planned)",
        our_module="engine/daemon.py (monitoring patterns)",
    ),
]


@dataclass
class UpstreamMonitor:
    """Monitors upstream projects for new releases and features.
    
    Usage:
        monitor = UpstreamMonitor()
        updates = monitor.check_all()
        for u in updates:
            print(f"{u['project']}: {u['summary']}")
    """
    state_path: str = ""
    projects: list = field(default_factory=lambda: list(UPSTREAM_PROJECTS))
    
    def __post_init__(self):
        if not self.state_path:
            state_dir = os.path.expanduser("~/.forgefleet")
            os.makedirs(state_dir, exist_ok=True)
            self.state_path = os.path.join(state_dir, "upstream_state.json")
        self._load_state()
    
    def _load_state(self):
        """Load last-checked state from disk."""
        if os.path.exists(self.state_path):
            try:
                with open(self.state_path) as f:
                    state = json.load(f)
                for project in self.projects:
                    if project.name in state:
                        s = state[project.name]
                        project.last_checked_sha = s.get("sha", "")
                        project.last_checked_tag = s.get("tag", "")
                        project.last_check_time = s.get("time", 0)
            except Exception:
                pass
    
    def _save_state(self):
        """Save current state to disk."""
        state = {}
        for p in self.projects:
            state[p.name] = {
                "sha": p.last_checked_sha,
                "tag": p.last_checked_tag,
                "time": p.last_check_time,
            }
        with open(self.state_path, "w") as f:
            json.dump(state, f, indent=2)
    
    def check_project(self, project: UpstreamProject) -> dict:
        """Check a single project for updates.
        
        Returns:
            dict with project name, has_update, new releases, recent commits summary
        """
        result = {
            "project": project.name,
            "repo": project.repo,
            "has_update": False,
            "latest_tag": "",
            "latest_sha": "",
            "new_releases": [],
            "recent_commits": [],
            "our_module": project.our_module,
            "what_we_took": project.what_we_took,
        }
        
        # Check latest release
        try:
            req = urllib.request.Request(
                f"https://api.github.com/repos/{project.repo}/releases/latest",
                headers={"Accept": "application/vnd.github.v3+json"},
            )
            with urllib.request.urlopen(req, timeout=10) as resp:
                release = json.loads(resp.read())
                result["latest_tag"] = release.get("tag_name", "")
                
                if project.last_checked_tag and result["latest_tag"] != project.last_checked_tag:
                    result["has_update"] = True
                    result["new_releases"].append({
                        "tag": result["latest_tag"],
                        "name": release.get("name", ""),
                        "date": release.get("published_at", ""),
                        "body": release.get("body", "")[:500],
                    })
        except Exception:
            pass
        
        # Check latest commits
        try:
            req = urllib.request.Request(
                f"https://api.github.com/repos/{project.repo}/commits?per_page=5",
                headers={"Accept": "application/vnd.github.v3+json"},
            )
            with urllib.request.urlopen(req, timeout=10) as resp:
                commits = json.loads(resp.read())
                result["latest_sha"] = commits[0]["sha"] if commits else ""
                
                if project.last_checked_sha and result["latest_sha"] != project.last_checked_sha:
                    result["has_update"] = True
                
                for c in commits:
                    result["recent_commits"].append({
                        "sha": c["sha"][:8],
                        "message": c["commit"]["message"].split("\n")[0][:100],
                        "date": c["commit"]["committer"]["date"],
                    })
        except Exception:
            pass
        
        # Update state
        if result["latest_sha"]:
            project.last_checked_sha = result["latest_sha"]
        if result["latest_tag"]:
            project.last_checked_tag = result["latest_tag"]
        project.last_check_time = time.time()
        
        return result
    
    def check_all(self) -> list[dict]:
        """Check all upstream projects for updates."""
        results = []
        for project in self.projects:
            try:
                result = self.check_project(project)
                results.append(result)
            except Exception as e:
                results.append({
                    "project": project.name,
                    "error": str(e),
                })
        
        self._save_state()
        return results
    
    def get_changelog_diff(self, project_name: str) -> str:
        """Get a summary of what changed since we last checked.
        
        Useful for deciding what new features to absorb.
        """
        project = next((p for p in self.projects if p.name == project_name), None)
        if not project:
            return f"Unknown project: {project_name}"
        
        result = self.check_project(project)
        
        lines = [f"## {project.name} ({project.repo})"]
        lines.append(f"License: {project.license}")
        lines.append(f"We use: {project.what_we_took}")
        lines.append(f"Our module: {project.our_module}")
        
        if result.get("new_releases"):
            lines.append(f"\n### New Release: {result['new_releases'][0]['tag']}")
            lines.append(result["new_releases"][0].get("body", "")[:500])
        
        if result.get("recent_commits"):
            lines.append("\n### Recent Commits:")
            for c in result["recent_commits"][:5]:
                lines.append(f"  {c['sha']} {c['message']}")
        
        return "\n".join(lines)
    
    def status_report(self) -> str:
        """Generate a status report of all upstream projects."""
        lines = ["# ForgeFleet Upstream Monitor", ""]
        lines.append(f"Tracking {len(self.projects)} projects:\n")
        
        for p in self.projects:
            last_check = ""
            if p.last_check_time:
                age = time.time() - p.last_check_time
                if age < 3600:
                    last_check = f"{int(age/60)}m ago"
                elif age < 86400:
                    last_check = f"{int(age/3600)}h ago"
                else:
                    last_check = f"{int(age/86400)}d ago"
            
            tag = f" [{p.last_checked_tag}]" if p.last_checked_tag else ""
            lines.append(f"- **{p.name}** ({p.repo}){tag}")
            lines.append(f"  Extracted: {p.what_we_took}")
            lines.append(f"  Our code: {p.our_module}")
            if last_check:
                lines.append(f"  Last checked: {last_check}")
            lines.append("")
        
        return "\n".join(lines)
