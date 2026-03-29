"""Self-Update — ForgeFleet modifies its own code to fix issues.

The loop:
1. Read evolution insights (what's broken)
2. Use local LLMs to write the fix
3. Test the fix
4. If passes → commit + restart daemon
5. If fails → revert
6. Log everything

ForgeFleet has all the tools: LLMs, write_file, git, run_command.
It just needs to point at its OWN repo.
"""
import json
import os
import subprocess
import time
from dataclasses import dataclass, field
from .evolution import EvolutionEngine
from .seniority import SeniorityPipeline
from .fleet_router import FleetRouter
from .tool import Tool
from .git_ops import GitOps
from .resilience import ResilienceManager, BuildLog


FORGEFLEET_REPO = os.path.expanduser("~/taylorProjects/forge-fleet")


@dataclass
class SelfUpdateResult:
    """Result of a self-update attempt."""
    insight: str
    fix_description: str
    files_changed: list = field(default_factory=list)
    tests_passed: bool = False
    deployed: bool = False
    reverted: bool = False
    error: str = ""


class SelfUpdater:
    """ForgeFleet updates its own code based on evolution insights.
    
    Safety guardrails:
    - Cannot delete core modules (agent.py, llm.py, tool.py, etc.)
    - Must pass tests before deploying
    - Auto-reverts if tests fail
    - Logs every change
    - Max 1 self-update per hour (prevent runaway loops)
    """
    
    PROTECTED_FILES = [
        "forgefleet/engine/agent.py",
        "forgefleet/engine/llm.py",
        "forgefleet/engine/tool.py",
        "forgefleet/engine/crew.py",
        "forgefleet/engine/task.py",
        "forgefleet/engine/self_update.py",  # Can't modify itself
        "forgefleet/server.py",
        "forgefleet/mcp_server.py",
    ]
    
    def __init__(self):
        self.repo = FORGEFLEET_REPO
        self.git = GitOps(self.repo)
        self.router = FleetRouter()
        self.evolution = EvolutionEngine()
        self.resilience = ResilienceManager()
        self.last_update = 0
    
    def apply_error_learnings(self):
        """Read top errors and update prompt templates to prevent them."""
        from .prompt_templates import TEMPLATES
        
        # Get top errors
        rows = self.evolution.db.execute(
            "SELECT error, COUNT(*) as cnt FROM task_records WHERE success=0 AND error != '' GROUP BY error ORDER BY cnt DESC LIMIT 5"
        ).fetchall()
        
        if not rows:
            return
        
        # Build "things to avoid" list from real errors
        avoid_list = []
        for error, count in rows:
            if "Python" in error or "Flask" in error:
                avoid_list.append("NEVER write Python/Flask code in a Rust project")
            if "HTML" in error or "CSS" in error:
                avoid_list.append("NEVER write plain HTML — use React components")
            if "described instead of writing" in error:
                avoid_list.append("ALWAYS use write_file tool — never just describe code")
            if "junk" in error.lower() or "wrong path" in error.lower():
                avoid_list.append("Write files to rust-backend/crates/ not src/")
        
        if avoid_list:
            # Update all prompt templates with learned rules
            for name, template in TEMPLATES.items():
                for rule in avoid_list:
                    if rule not in template.system_prompt:
                        template.system_prompt += f"\n⚠️ LEARNED RULE: {rule}"
            
            print(f"  📝 Applied {len(avoid_list)} learned rules to prompt templates", flush=True)
            
            # Mark insights as applied
            self.evolution.db.execute("UPDATE insights SET applied=1 WHERE applied=0")
            self.evolution.db.commit()
    
    def run_improvement_cycle(self) -> list[SelfUpdateResult]:
        """Run one self-improvement cycle.
        
        1. Check evolution insights
        2. Pick the highest-priority fixable issue
        3. Write the fix
        4. Test it
        5. Deploy or revert
        """
        # Rate limit: max 1 update per hour
        if time.time() - self.last_update < 3600:
            return []
        
        results = []
        
        # Get unresolved insights
        insights = self.evolution.db.execute(
            "SELECT id, category, finding, recommendation FROM insights WHERE applied=0 ORDER BY confidence DESC LIMIT 3"
        ).fetchall()
        
        if not insights:
            return []
        
        for insight_row in insights[:1]:  # One fix at a time
            insight_id, category, finding, recommendation = insight_row
            
            print(f"\n🔧 Self-Update: [{category}] {finding[:60]}", flush=True)
            print(f"   Fix: {recommendation[:60]}", flush=True)
            
            result = self._attempt_fix(finding, recommendation)
            results.append(result)
            
            if result.deployed:
                # Mark insight as applied
                self.evolution.db.execute(
                    "UPDATE insights SET applied=1 WHERE id=?", (insight_id,)
                )
                self.evolution.db.commit()
                self.last_update = time.time()
                
                # Log it
                self.resilience.log_build(BuildLog(
                    timestamp=time.strftime("%Y-%m-%dT%H:%M:%S"),
                    ticket_id=f"self-update-{insight_id}",
                    title=f"Self-fix: {finding[:50]}",
                    status="completed",
                    branch=f"self-update-{insight_id}",
                ))
        
        return results
    
    def _attempt_fix(self, finding: str, recommendation: str) -> SelfUpdateResult:
        """Attempt to fix an issue using the seniority pipeline."""
        result = SelfUpdateResult(insight=finding, fix_description=recommendation)
        
        # Build tools scoped to ForgeFleet repo
        tools = self._build_tools()
        
        # Use the seniority pipeline on ForgeFleet's own code
        pipeline = SeniorityPipeline(tools=tools, router=self.router)
        
        task = (
            f"Fix this issue in ForgeFleet's Python codebase:\n\n"
            f"Problem: {finding}\n"
            f"Recommended fix: {recommendation}\n\n"
            f"The code is in forgefleet/engine/. "
            f"Read the relevant files, make the fix, and save with write_file.\n"
            f"DO NOT modify these protected files: {', '.join(os.path.basename(f) for f in self.PROTECTED_FILES[:5])}"
        )
        
        tech_stack = {"backend": "Python", "structure": "forgefleet/engine/ for all modules"}
        
        # Create a branch for the fix
        branch = f"self-update-{int(time.time())}"
        self.git.create_branch(branch)
        
        try:
            build_result = pipeline.execute(task, tech_stack=tech_stack)
            
            if not self.git.has_changes():
                result.error = "No code changes produced"
                self.git._run("checkout", "main")
                return result
            
            # Check for protected file modifications
            changed = subprocess.run(
                ["git", "diff", "--cached", "--name-only"],
                capture_output=True, text=True, cwd=self.repo,
            ).stdout.strip().split("\n")
            
            for f in changed:
                if f in self.PROTECTED_FILES:
                    result.error = f"Protected file modified: {f}"
                    self.git._run("checkout", "main")
                    return result
            
            result.files_changed = changed
            
            # Run tests
            result.tests_passed = self._run_tests()
            
            if result.tests_passed:
                # Commit + push
                self.git.stage_all()
                self.git.commit(f"self-update: {finding[:50]}")
                push = self.git.push(branch)
                
                if push.success:
                    # Merge to main
                    self.git._run("checkout", "main")
                    merge = self.git._run("merge", branch)
                    
                    if merge.success:
                        self.git.push("main")
                        result.deployed = True
                        print(f"   ✅ Self-update deployed!", flush=True)
                        
                        # Restart daemon to pick up changes
                        self._restart_daemon()
                    else:
                        result.error = f"Merge failed: {merge.error}"
                        self.git._run("merge", "--abort")
                else:
                    result.error = f"Push failed: {push.error}"
            else:
                result.error = "Tests failed — reverting"
                result.reverted = True
                self.git._run("checkout", "main")
                self.git._run("branch", "-D", branch)
                print(f"   ❌ Tests failed, reverted", flush=True)
                
        except Exception as e:
            result.error = str(e)
            self.git._run("checkout", "main")
        
        return result
    
    def _run_tests(self) -> bool:
        """Run ForgeFleet's own tests."""
        try:
            # Basic: verify all modules import correctly
            r = subprocess.run(
                ["python3.12", "-c", 
                 "from forgefleet.engine import Agent, Task, Crew, LLM, Tool; "
                 "from forgefleet.engine.fleet_router import FleetRouter; "
                 "from forgefleet.engine.seniority import SeniorityPipeline; "
                 "print('All imports OK')"],
                capture_output=True, text=True, timeout=30,
                cwd=self.repo,
                env={**os.environ, "PYTHONPATH": self.repo},
            )
            return r.returncode == 0 and "All imports OK" in r.stdout
        except Exception:
            return False
    
    def _restart_daemon(self):
        """Restart the ForgeFleet daemon to pick up code changes."""
        try:
            subprocess.run(
                ["launchctl", "unload", 
                 os.path.expanduser("~/Library/LaunchAgents/com.forgefleet.daemon.plist")],
                capture_output=True, timeout=10,
            )
            time.sleep(2)
            subprocess.run(
                ["launchctl", "load",
                 os.path.expanduser("~/Library/LaunchAgents/com.forgefleet.daemon.plist")],
                capture_output=True, timeout=10,
            )
            print(f"   🔄 Daemon restarted", flush=True)
        except Exception as e:
            print(f"   ⚠️ Daemon restart failed: {e}", flush=True)
    
    def check_and_restart_llms(self) -> list[str]:
        """Check LLM health and restart any that are down."""
        restarted = []
        
        for ep in self.router.endpoints:
            try:
                import urllib.request
                req = urllib.request.Request(f"http://{ep.ip}:{ep.port}/health")
                with urllib.request.urlopen(req, timeout=5) as resp:
                    data = json.loads(resp.read())
                    if data.get("status") == "ok":
                        continue
            except Exception:
                pass
            
            # Endpoint is down — try to restart
            print(f"   ⚠️ {ep.name} on {ep.ip}:{ep.port} is DOWN", flush=True)
            
            # Find the model path and restart
            success = self._restart_llm_endpoint(ep.ip, ep.port)
            if success:
                restarted.append(f"{ep.name} on {ep.ip}:{ep.port}")
                print(f"   ✅ Restarted {ep.name}", flush=True)
        
        return restarted
    
    def _restart_llm_endpoint(self, ip: str, port: int) -> bool:
        """Restart a specific LLM endpoint."""
        import socket
        local_ip = self._get_local_ip()
        
        # Find model path from fleet.json or known paths
        model_paths = {
            51803: "models/qwen3.5-9b/Qwen3.5-9B-Q4_K_M.gguf",
            51802: "models/qwen2.5-coder-32b/Qwen2.5-Coder-32B-Instruct-Q4_K_M.gguf",
            51801: "models/qwen2.5-72b/Qwen2.5-72B-Instruct-Q4_K_M.gguf",
        }
        
        model = model_paths.get(port)
        if not model:
            return False
        
        if ip == local_ip:
            home = os.path.expanduser("~")
            model_path = os.path.join(home, model)
            if os.path.exists(model_path):
                subprocess.Popen(
                    f"nohup llama-server --model {model_path} --port {port} --host 0.0.0.0 --ctx-size 8192 --n-gpu-layers 99 > /tmp/llama-{port}.log 2>&1 &",
                    shell=True,
                )
                time.sleep(5)
                return True
        else:
            # Remote node
            try:
                ssh_user = self._get_ssh_user(ip)
                subprocess.run(
                    ["ssh", f"{ssh_user}@{ip}" if ssh_user else ip,
                     f"bash -c 'nohup llama-server --model ~/{model} --port {port} --host 0.0.0.0 --ctx-size 8192 --n-gpu-layers 99 > /tmp/llama-{port}.log 2>&1 &'"],
                    capture_output=True, timeout=15,
                )
                time.sleep(5)
                return True
            except:
                pass
        
        return False
    
    def _get_local_ip(self) -> str:
        import socket
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            ip = s.getsockname()[0]; s.close(); return ip
        except: return "127.0.0.1"
    
    def _get_ssh_user(self, ip: str) -> str:
        """Get SSH user for an IP from fleet.json."""
        try:
            fleet_path = os.path.expanduser("~/.openclaw/workspace/fleet.json")
            with open(fleet_path) as f:
                fleet = json.load(f)
            for node in fleet.get("nodes", {}).values():
                if node.get("ip") == ip:
                    return node.get("ssh_user", "")
        except: pass
        return ""
    
    def _build_tools(self) -> list:
        """Build tools scoped to ForgeFleet repo."""
        repo = self.repo
        
        def rf(filepath=""):
            f = os.path.join(repo, filepath)
            if not os.path.exists(f): return f"Not found: {filepath}"
            c = open(f).read(); return c[:4000] if len(c) > 4000 else c
        
        def lf(directory=".", pattern=""):
            full = os.path.join(repo, directory)
            exclude = {"__pycache__", ".git", ".venv"}
            files = []
            for root, dirs, fnames in os.walk(full):
                dirs[:] = [d for d in dirs if d not in exclude]
                for f in fnames:
                    if pattern and not f.endswith(pattern): continue
                    files.append(os.path.relpath(os.path.join(root, f), repo))
                if len(files) > 30: break
            return "\n".join(files[:30])
        
        def wf(filepath="", content=""):
            # Guardrail: block protected files
            if filepath in self.PROTECTED_FILES:
                return f"REJECTED: {filepath} is protected and cannot be modified by self-update"
            f = os.path.join(repo, filepath)
            os.makedirs(os.path.dirname(f), exist_ok=True)
            open(f, "w").write(content)
            return f"WRITTEN: {filepath} ({len(content)} chars)"
        
        def rc(command=""):
            try:
                r = subprocess.run(command, shell=True, capture_output=True, text=True, timeout=60, cwd=repo)
                return (r.stdout + r.stderr)[:3000]
            except Exception as e: return str(e)
        
        return [
            Tool(name="read_file", description="Read a ForgeFleet source file",
                 parameters={"type": "object", "properties": {"filepath": {"type": "string"}}, "required": ["filepath"]}, func=rf),
            Tool(name="list_files", description="List ForgeFleet source files",
                 parameters={"type": "object", "properties": {"directory": {"type": "string"}, "pattern": {"type": "string"}}}, func=lf),
            Tool(name="write_file", description="Modify a ForgeFleet source file (protected files blocked)",
                 parameters={"type": "object", "properties": {"filepath": {"type": "string"}, "content": {"type": "string"}}, "required": ["filepath", "content"]}, func=wf),
            Tool(name="run_command", description="Run a command in the ForgeFleet repo",
                 parameters={"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}, func=rc),
        ]
