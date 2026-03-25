"""ForgeFleet v2 — CrewAI-powered coding agent pipeline.

Uses local LLMs via llama.cpp (OpenAI-compatible API).
Each agent role uses a different model tier from the fleet.
"""
import os
import json
import subprocess
from pathlib import Path
from crewai import Agent, Crew, Task, Process, LLM
from crewai.tools import BaseTool
from typing import Type
from pydantic import BaseModel, Field


# ─── Fleet-aware LLM configuration ─────────────────────

def load_fleet_config() -> dict:
    """Load fleet.json to discover available models."""
    for path in [
        os.path.expanduser("~/fleet.json"),
        os.path.expanduser("~/.openclaw/workspace/fleet.json"),
        "/Users/venkat/.openclaw/workspace/fleet.json",
    ]:
        if os.path.exists(path):
            with open(path) as f:
                return json.load(f)
    return {}


def get_llm(tier: str = "fast") -> LLM:
    """Get an LLM configured for a specific tier.
    
    Tiers:
        fast   — 9B models (context gathering, scaffolding)
        code   — 32B models (code writing, editing)
        review — 72B models (code review, complex reasoning)
        expert — 235B cluster (hardest problems)
    """
    fleet = load_fleet_config()
    
    # Default endpoints based on our fleet layout
    endpoints = {
        "fast": "http://192.168.5.100:8082",    # Taylor 9B
        "code": "http://192.168.5.102:8081",     # Marcus 32B
        "review": "http://192.168.5.108:8082",   # James 72B
        "expert": "http://192.168.5.100:8080",   # Taylor 235B cluster
    }
    
    model_names = {
        "fast": "qwen3.5-9b",
        "code": "qwen2.5-coder-32b",
        "review": "qwen2.5-72b",
        "expert": "qwen3-235b",
    }
    
    # Try to read from fleet.json
    if fleet and "nodes" in fleet:
        for name, node in fleet["nodes"].items():
            for model in node.get("models", []):
                t = model.get("tier", 0)
                url = f"http://{node.get('ip', '127.0.0.1')}:{model.get('port', 8080)}"
                if t == 1 and tier == "fast":
                    endpoints["fast"] = url
                elif t == 2 and tier == "code":
                    endpoints["code"] = url
                elif t == 3 and tier == "review":
                    endpoints["review"] = url
                elif t == 4 and tier == "expert":
                    endpoints["expert"] = url
    
    base_url = endpoints.get(tier, endpoints["fast"])
    model_name = model_names.get(tier, "qwen3.5-9b")
    
    return LLM(
        model=f"openai/{model_name}",
        base_url=f"{base_url}/v1",
        api_key="not-needed",
        temperature=0.2,
        timeout=900,  # 15 min for large models
    )


# ─── Custom Tools ───────────────────────────────────────

class FileReadInput(BaseModel):
    filepath: str = Field(description="Path to the file to read")

class FileReadTool(BaseTool):
    name: str = "read_file"
    description: str = "Read the contents of a file in the repository"
    args_schema: Type[BaseModel] = FileReadInput
    repo_dir: str = "."
    
    def _run(self, filepath: str) -> str:
        full_path = os.path.join(self.repo_dir, filepath)
        if not os.path.exists(full_path):
            return f"File not found: {filepath}"
        try:
            content = Path(full_path).read_text()
            if len(content) > 8000:
                return content[:4000] + "\n\n... [truncated] ...\n\n" + content[-4000:]
            return content
        except Exception as e:
            return f"Error reading {filepath}: {e}"


class FileWriteInput(BaseModel):
    filepath: str = Field(description="Path to create/overwrite")
    content: str = Field(description="File content to write")

class FileWriteTool(BaseTool):
    name: str = "write_file"
    description: str = "Write content to a file, creating directories if needed"
    args_schema: Type[BaseModel] = FileWriteInput
    repo_dir: str = "."
    
    def _run(self, filepath: str, content: str) -> str:
        full_path = os.path.join(self.repo_dir, filepath)
        try:
            os.makedirs(os.path.dirname(full_path), exist_ok=True)
            Path(full_path).write_text(content)
            return f"Written: {filepath} ({len(content)} chars)"
        except Exception as e:
            return f"Error writing {filepath}: {e}"


class ShellInput(BaseModel):
    command: str = Field(description="Shell command to execute")

class ShellTool(BaseTool):
    name: str = "run_command"
    description: str = "Run a shell command in the repository directory (for cargo check, tests, etc.)"
    args_schema: Type[BaseModel] = ShellInput
    repo_dir: str = "."
    
    def _run(self, command: str) -> str:
        try:
            r = subprocess.run(
                command, shell=True, capture_output=True, text=True,
                timeout=120, cwd=self.repo_dir
            )
            output = r.stdout + r.stderr
            if len(output) > 4000:
                output = output[:2000] + "\n...[truncated]...\n" + output[-2000:]
            return f"Exit code: {r.returncode}\n{output}"
        except subprocess.TimeoutExpired:
            return "Command timed out after 120 seconds"
        except Exception as e:
            return f"Error: {e}"


class ListFilesInput(BaseModel):
    directory: str = Field(description="Directory to list", default=".")
    pattern: str = Field(description="File extension to filter", default="")

class ListFilesTool(BaseTool):
    name: str = "list_files"
    description: str = "List files in a directory, optionally filtered by extension"
    args_schema: Type[BaseModel] = ListFilesInput
    repo_dir: str = "."
    
    def _run(self, directory: str = ".", pattern: str = "") -> str:
        full_path = os.path.join(self.repo_dir, directory)
        exclude = {'target', 'node_modules', '.git', 'dist', '.next', '__pycache__'}
        files = []
        for root, dirs, filenames in os.walk(full_path):
            dirs[:] = [d for d in dirs if d not in exclude]
            for f in filenames:
                if pattern and not f.endswith(pattern):
                    continue
                rel = os.path.relpath(os.path.join(root, f), self.repo_dir)
                files.append(rel)
        return "\n".join(sorted(files)[:200])


# ─── Build the Crew ─────────────────────────────────────

def build_coding_crew(repo_dir: str, task_title: str, task_description: str) -> Crew:
    """Build a coding crew for a specific task.
    
    Three agents, three tiers:
    1. Context Engineer (9B) — fast, finds relevant code
    2. Code Writer (32B) — quality code generation
    3. Code Reviewer (72B) — catches bugs, verifies quality
    """
    
    # Tools scoped to the repo
    read_file = FileReadTool(repo_dir=repo_dir)
    write_file = FileWriteTool(repo_dir=repo_dir)
    shell = ShellTool(repo_dir=repo_dir)
    list_files = ListFilesTool(repo_dir=repo_dir)
    
    # Agent 1: Context Engineer — fast model, reads a lot
    context_engineer = Agent(
        role="Context Engineer",
        goal="Find all relevant code, architecture patterns, and existing implementations related to the task",
        backstory=(
            "You are an expert at navigating large codebases. "
            "You quickly identify the relevant files, understand the architecture, "
            "and provide a focused summary of what exists and what needs to change. "
            "You never write code — you only research and report."
        ),
        tools=[read_file, list_files, shell],
        llm=get_llm("fast"),
        verbose=True,
        allow_delegation=False,
    )
    
    # Agent 2: Code Writer — stronger model, writes code
    code_writer = Agent(
        role="Senior Rust/TypeScript Developer",
        goal="Write production-quality code that compiles, has error handling, and follows project conventions",
        backstory=(
            "You are a senior developer who writes clean, production-ready code. "
            "You NEVER use placeholders, TODOs, or stub implementations. "
            "Every function has proper error handling. Every public function has doc comments. "
            "You write the COMPLETE implementation, not scaffolding."
        ),
        tools=[read_file, write_file, shell, list_files],
        llm=get_llm("code"),
        verbose=True,
        allow_delegation=False,
    )
    
    # Agent 3: Code Reviewer — strongest model available
    code_reviewer = Agent(
        role="Code Reviewer",
        goal="Review the code changes for bugs, missing error handling, security issues, and verify it compiles",
        backstory=(
            "You are a meticulous code reviewer. You check: "
            "1) Does it compile? (run cargo check / npm run build) "
            "2) Any placeholder code? (reject TODO, unimplemented!, let _ =) "
            "3) Error handling on every external call? "
            "4) Security issues? "
            "5) Does it match the task requirements? "
            "If issues found, provide specific fixes."
        ),
        tools=[read_file, shell, list_files],
        llm=get_llm("review"),
        verbose=True,
        allow_delegation=False,
    )
    
    # Tasks
    gather_context = Task(
        description=f"""Research the codebase for task: "{task_title}"

Task details: {task_description}

List relevant files, their purposes, and how they connect.
Identify the exact files that need to be created or modified.
Note any patterns, conventions, or dependencies to follow.
""",
        expected_output="A structured summary of: relevant files, architecture patterns, dependencies, and specific changes needed",
        agent=context_engineer,
    )
    
    write_code = Task(
        description=f"""Implement the following task using the context from the research phase:

Task: {task_title}
Details: {task_description}

Requirements:
- Write COMPLETE, production-ready code
- NO placeholders, TODOs, or stubs
- Proper error handling on every external call
- Doc comments on public functions
- Follow existing project patterns and conventions
""",
        expected_output="All files created/modified with their complete content. A summary of what was changed and why.",
        agent=code_writer,
        context=[gather_context],
    )
    
    review_code = Task(
        description=f"""Review the code changes made for: "{task_title}"

Check:
1. Run `cargo check` or `npm run build` to verify compilation
2. Look for placeholder code (TODO, unimplemented!, stub)
3. Verify error handling on every external call
4. Check for security issues
5. Verify the implementation matches the task requirements
6. Check that all files are properly connected (imports, routes, etc.)

If issues found, fix them directly.
""",
        expected_output="Review results: PASS or FAIL with specific issues. If FAIL, what was fixed.",
        agent=code_reviewer,
        context=[gather_context, write_code],
    )
    
    return Crew(
        agents=[context_engineer, code_writer, code_reviewer],
        tasks=[gather_context, write_code, review_code],
        process=Process.sequential,
        verbose=True,
    )
