"""Autonomous Worker — runs independently without OpenClaw.

If OpenClaw goes down, ForgeFleet keeps building.
Claims tickets from MC, runs crews with local LLMs,
commits code, updates tickets — all without any external orchestrator.

This is the "brain" that makes the fleet truly autonomous.
"""
import os
import signal
import subprocess
import time
import traceback
from dataclasses import dataclass, field
from .agent import Agent
from .task import Task
from .crew import Crew
from .llm import LLM
from .tool import Tool
from .fleet_router import FleetRouter
from .mc_client import MCClient
from .git_ops import GitOps
from .self_improve import SelfImprover, Learning
from .prompt_templates import get_template
from .output_validator import OutputValidator
from .gap_analyzer import GapAnalyzer
from .task_decomposer import TaskDecomposer
from .skill_loader import SkillLoader
from .scheduler import AutoScheduler, SchedulerConfig


@dataclass
class AutonomousWorker:
    """Fully autonomous coding agent — no OpenClaw required.
    
    Loop:
    1. Check if user is idle (scheduler)
    2. Claim a ticket from MC
    3. Decompose if complex
    4. Run crew (Context → Code → Review → Gap Analysis)
    5. Commit + push
    6. Update ticket in MC
    7. Learn from outcome
    8. Repeat
    """
    repo_dir: str = "/Users/venkat/taylorProjects/HireFlow360"
    mc_url: str = "http://192.168.5.100:60002"
    node_name: str = "taylor-forgefleet"
    poll_interval: int = 30  # seconds between ticket checks
    max_tasks_per_session: int = 20
    only_when_idle: bool = True
    
    # Components
    router: FleetRouter = field(default_factory=FleetRouter)
    mc: MCClient = field(default_factory=lambda: MCClient())
    git: GitOps = None
    improver: SelfImprover = field(default_factory=SelfImprover)
    validator: OutputValidator = field(default_factory=OutputValidator)
    decomposer: TaskDecomposer = None
    skills: SkillLoader = field(default_factory=SkillLoader)
    scheduler: AutoScheduler = field(default_factory=lambda: AutoScheduler(
        config=SchedulerConfig(idle_short_seconds=300, night_start_hour=23, night_end_hour=8)
    ))
    
    _running: bool = False
    _tasks_completed: int = 0
    
    def __post_init__(self):
        self.mc.base_url = self.mc_url
        self.mc.node_name = self.node_name
        self.git = GitOps(self.repo_dir)
        
        llm_fast = self.router.get_llm(1) or LLM(base_url="http://192.168.5.100:51803/v1")
        self.decomposer = TaskDecomposer(llm=llm_fast)
    
    def run(self):
        """Main autonomous loop."""
        self._running = True
        
        # Handle shutdown gracefully
        def shutdown(signum, frame):
            print("\n⛔ Autonomous worker shutting down...")
            self._running = False
        
        signal.signal(signal.SIGTERM, shutdown)
        signal.signal(signal.SIGINT, shutdown)
        
        print(f"🤖 ForgeFleet Autonomous Worker starting")
        print(f"   Repo: {self.repo_dir}")
        print(f"   MC: {self.mc_url}")
        print(f"   Node: {self.node_name}")
        print(f"   Idle-only: {self.only_when_idle}")
        print(f"   Skills loaded: {self.skills.stats()['total_skills']}")
        
        while self._running and self._tasks_completed < self.max_tasks_per_session:
            try:
                # Check if we should be working
                if self.only_when_idle:
                    state = self.scheduler.determine_state()
                    if state.value == "active":
                        time.sleep(self.poll_interval)
                        continue
                
                # Check MC health
                if not self.mc.health():
                    print("  ⚠️ MC unreachable, waiting...")
                    time.sleep(60)
                    continue
                
                # Claim a ticket
                ticket = self._claim_next_ticket()
                if not ticket:
                    time.sleep(self.poll_interval)
                    continue
                
                # Work on it
                success = self._work_on_ticket(ticket)
                
                if success:
                    self._tasks_completed += 1
                    print(f"  ✅ Task #{self._tasks_completed} complete")
                
            except KeyboardInterrupt:
                break
            except Exception as e:
                print(f"  ❌ Error: {e}")
                traceback.print_exc()
                time.sleep(30)
        
        print(f"⛔ Worker stopped. Completed {self._tasks_completed} tasks.")
    
    def _claim_next_ticket(self) -> dict:
        """Get the next claimable ticket from MC."""
        tickets = self.mc.get_claimable()
        if not tickets:
            return None
        
        ticket = tickets[0]
        self.mc.claim_ticket(ticket["id"])
        print(f"\n📋 Claimed: {ticket['title'][:60]}")
        return ticket
    
    def _work_on_ticket(self, ticket: dict) -> bool:
        """Complete a ticket using the full ForgeFleet pipeline."""
        ticket_id = ticket["id"]
        title = ticket.get("title", "")
        description = ticket.get("description", title)
        
        start_time = time.time()
        
        try:
            # 1. Create git branch
            branch = f"feat/forgefleet-{ticket_id[:8]}"
            self.git.create_branch(branch)
            
            # 2. Decompose if complex
            subtasks = self.decomposer.decompose(description)
            if len(subtasks) > 1:
                print(f"  📋 Decomposed into {len(subtasks)} subtasks")
            
            # 3. Run crew for each subtask
            all_output = ""
            for i, subtask in enumerate(subtasks):
                print(f"  🔨 Subtask {i+1}/{len(subtasks)}: {subtask.title[:50]}")
                
                output = self._run_crew(subtask.description)
                all_output += f"\n## {subtask.title}\n{output}\n"
            
            # 4. Gap analysis
            analyzer = GapAnalyzer(
                llm=self.router.get_llm(3) or LLM(base_url="http://192.168.5.100:51801/v1"),
                repo_dir=self.repo_dir,
            )
            review = analyzer.full_review(description, all_output)
            
            if review["verdict"] == "NEEDS_WORK" and review["gaps"]["critical"] > 0:
                print(f"  ⚠️ {review['gaps']['critical']} critical gaps — attempting fix...")
                # Try to fix critical gaps
                gap_descriptions = "\n".join(
                    g["desc"] for g in review["gaps"]["items"] if g["sev"] == "critical"
                )
                fix_output = self._run_crew(
                    f"Fix these issues in the code:\n{gap_descriptions}\n\nOriginal task: {description}"
                )
                all_output += f"\n## Fixes\n{fix_output}\n"
            
            # 5. Commit + push
            if self.git.has_changes():
                self.git.stage_all()
                self.git.commit(f"feat: {title[:50]} [ForgeFleet]")
                push_result = self.git.push(branch)
                
                if push_result.success:
                    # 6. Update MC
                    self.mc.complete_ticket(
                        ticket_id,
                        result=f"Completed by ForgeFleet.\nBranch: {branch}\n{all_output[:1000]}",
                        branch=branch,
                    )
                    
                    # Create review ticket
                    self.mc.create_review_ticket(
                        ticket_id, branch, title,
                        f"Auto-generated by ForgeFleet. Review: {review['verdict']}"
                    )
                    
                    # Record success
                    self.improver.record(Learning(
                        task_type="autonomous_build",
                        model_used="fleet",
                        tier=0,
                        outcome="success",
                        duration_seconds=time.time() - start_time,
                    ))
                    
                    return True
            
            # No changes or push failed
            self.mc.fail_ticket(ticket_id, "No code changes produced or push failed")
            self.improver.record(Learning(
                task_type="autonomous_build", model_used="fleet",
                tier=0, outcome="failure",
                error_pattern="no_changes_produced",
                duration_seconds=time.time() - start_time,
            ))
            return False
            
        except Exception as e:
            self.mc.fail_ticket(ticket_id, str(e)[:500])
            self.improver.record(Learning(
                task_type="autonomous_build", model_used="fleet",
                tier=0, outcome="failure",
                error_pattern=str(e)[:200],
                duration_seconds=time.time() - start_time,
            ))
            return False
        finally:
            # Always return to main branch
            try:
                self.git._run("checkout", "main")
            except Exception:
                pass
    
    def _run_crew(self, task_description: str) -> str:
        """Run a 3-agent crew on a task."""
        llm_fast = self.router.get_llm(1) or LLM(base_url="http://192.168.5.100:51803/v1")
        llm_code = self.router.get_llm(2) or LLM(base_url="http://192.168.5.100:51802/v1")
        llm_review = self.router.get_llm(3) or LLM(base_url="http://192.168.5.100:51801/v1")
        
        # Get task-specific template + skill injection
        template = get_template(task_description)
        skill_context = self.skills.inject_into_prompt(task_description, max_chars=2000)
        
        # Build tools
        tools = self._build_tools()
        
        researcher = Agent(
            role="Context Engineer", goal="Find relevant code for the task",
            backstory="You scan codebases quickly.", 
            tools=[tools[0], tools[1], tools[3]],
            llm=llm_fast, verbose=True, max_iterations=8,
        )
        
        coder = Agent(
            role="Senior Developer",
            goal="Write production-quality code",
            backstory=template.system_prompt + ("\n\n" + skill_context if skill_context else ""),
            tools=tools, llm=llm_code, verbose=True, max_iterations=10,
        )
        
        reviewer = Agent(
            role="Code Reviewer", goal="Verify code quality",
            backstory="You catch bugs, missing error handling, and placeholder code.",
            tools=[tools[0], tools[1], tools[3]],
            llm=llm_review, verbose=True, max_iterations=6,
        )
        
        t1 = Task(description=f"Research: {task_description}", agent=researcher)
        t2 = Task(description=f"Implement: {task_description}", agent=coder, context_tasks=[t1])
        t3 = Task(description=f"Review: {task_description}", agent=reviewer, context_tasks=[t1, t2])
        
        crew = Crew(agents=[researcher, coder, reviewer], tasks=[t1, t2, t3], verbose=True)
        result = crew.kickoff()
        
        return result.get("final_output", "")
    
    def _build_tools(self) -> list:
        """Build file tools scoped to the repo."""
        repo = self.repo_dir
        
        def read_file(filepath=""):
            full = os.path.join(repo, filepath)
            if not os.path.exists(full): return f"Not found: {filepath}"
            c = open(full).read()
            return c[:6000] if len(c) > 6000 else c
        
        def write_file(filepath="", content=""):
            full = os.path.join(repo, filepath)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            open(full, "w").write(content)
            return f"Written: {filepath} ({len(content)} chars)"
        
        def list_files(directory=".", pattern=""):
            full = os.path.join(repo, directory)
            exclude = {"target","node_modules",".git","dist",".next","__pycache__"}
            files = []
            for root, dirs, fnames in os.walk(full):
                dirs[:] = [d for d in dirs if d not in exclude]
                for f in fnames:
                    if pattern and not f.endswith(pattern): continue
                    files.append(os.path.relpath(os.path.join(root, f), repo))
                if len(files) > 50: break
            return "\n".join(files[:50])
        
        def run_cmd(command=""):
            try:
                r = subprocess.run(command, shell=True, capture_output=True, text=True, timeout=60, cwd=repo)
                out = r.stdout + r.stderr
                return out[:4000] if len(out) > 4000 else out
            except Exception as e: return f"Error: {e}"
        
        return [
            Tool(name="read_file", description="Read a file",
                 parameters={"type":"object","properties":{"filepath":{"type":"string"}}}, func=read_file),
            Tool(name="list_files", description="List files",
                 parameters={"type":"object","properties":{"directory":{"type":"string"},"pattern":{"type":"string"}}}, func=list_files),
            Tool(name="write_file", description="Write a file",
                 parameters={"type":"object","properties":{"filepath":{"type":"string"},"content":{"type":"string"}}}, func=write_file),
            Tool(name="run_command", description="Run shell command",
                 parameters={"type":"object","properties":{"command":{"type":"string"}}}, func=run_cmd),
        ]
    
    def status(self) -> dict:
        """Get worker status."""
        return {
            "running": self._running,
            "tasks_completed": self._tasks_completed,
            "repo": self.repo_dir,
            "mc_healthy": self.mc.health(),
            "fleet_endpoints": len(self.router.endpoints),
            "skills_loaded": self.skills.stats()["total_skills"],
            "learnings": self.improver.stats(),
        }


# ─── Entry point for standalone execution ──────────────

if __name__ == "__main__":
    import sys
    
    repo = sys.argv[1] if len(sys.argv) > 1 else "/Users/venkat/taylorProjects/HireFlow360"
    
    worker = AutonomousWorker(repo_dir=repo)
    worker.run()
