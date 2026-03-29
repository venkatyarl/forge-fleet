"""Lifecycle Manager — the master loop that never stops.

ForgeFleet's complete autonomous lifecycle:
1. WORK — build tickets
2. LEARN — record outcomes
3. ANALYZE — find issues after every batch
4. SELF-UPDATE — stop, fix itself, restart
5. VERIFY — retry failed tasks to confirm the fix worked
6. RESEARCH — if idle, study and improve

This is the top-level orchestrator that runs forever.
"""
import json
import os
import signal
import sys
import time
import traceback
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from .autonomous import AutonomousWorker
from .evolution import EvolutionEngine, TaskRecord
from .self_update import SelfUpdater
from .resilience import ResilienceManager, BuildLog
from .research import ResearchEngine
from .continuous_improvement import ContinuousImprover
from .scheduler import AutoScheduler, ActivityState
from .openclaw_bridge import OpenClawBridge


class Phase(Enum):
    WORK = "work"
    LEARN = "learn"
    ANALYZE = "analyze"
    SELF_UPDATE = "self_update"
    VERIFY = "verify"
    RESEARCH = "research"
    IDLE = "idle"


@dataclass
class LifecycleState:
    """Persistent state that survives restarts."""
    phase: str = "work"
    tasks_since_last_analyze: int = 0
    failed_task_ids: list = field(default_factory=list)  # Tasks to retry after update
    last_self_update: float = 0
    last_research: float = 0
    total_cycles: int = 0
    
    STATE_FILE = os.path.expanduser("~/.forgefleet/lifecycle_state.json")
    
    def save(self):
        os.makedirs(os.path.dirname(self.STATE_FILE), exist_ok=True)
        with open(self.STATE_FILE, "w") as f:
            json.dump({
                "phase": self.phase,
                "tasks_since_analyze": self.tasks_since_last_analyze,
                "failed_task_ids": self.failed_task_ids[-20:],  # Keep last 20
                "last_self_update": self.last_self_update,
                "last_research": self.last_research,
                "total_cycles": self.total_cycles,
            }, f)
    
    @classmethod
    def load(cls) -> "LifecycleState":
        state = cls()
        if os.path.exists(cls.STATE_FILE):
            try:
                with open(cls.STATE_FILE) as f:
                    data = json.load(f)
                state.phase = data.get("phase", "work")
                state.tasks_since_last_analyze = data.get("tasks_since_analyze", 0)
                state.failed_task_ids = data.get("failed_task_ids", [])
                state.last_self_update = data.get("last_self_update", 0)
                state.last_research = data.get("last_research", 0)
                state.total_cycles = data.get("total_cycles", 0)
            except:
                pass
        return state


class LifecycleManager:
    """The master loop — ForgeFleet never stops improving.
    
    Cycle:
    WORK (5 tasks) → LEARN → ANALYZE → SELF-UPDATE → VERIFY → RESEARCH → repeat
    
    If idle (no tickets): → RESEARCH → SELF-UPDATE → wait
    """
    
    ANALYZE_EVERY_N_TASKS = 5  # Analyze after every 5 tasks
    RESEARCH_INTERVAL = 14400  # Research every 4 hours
    SELF_UPDATE_INTERVAL = 3600  # Self-update at most every hour
    
    def __init__(self, repo_dir: str = ""):
        self.state = LifecycleState.load()
        self.worker = AutonomousWorker(repo_dir=repo_dir) if repo_dir else None
        self.evolution = EvolutionEngine()
        self.updater = SelfUpdater()
        self.resilience = ResilienceManager()
        self.researcher = ResearchEngine()
        self.improver = ContinuousImprover()
        self.scheduler = AutoScheduler()
        self.notify = OpenClawBridge()
        self._running = False
    
    def run(self):
        """The infinite loop."""
        self._running = True
        
        signal.signal(signal.SIGTERM, lambda s, f: self._shutdown())
        signal.signal(signal.SIGINT, lambda s, f: self._shutdown())
        
        print(f"🔄 ForgeFleet Lifecycle starting (cycle #{self.state.total_cycles})", flush=True)
        print(f"   Phase: {self.state.phase}", flush=True)
        print(f"   Failed tasks to retry: {len(self.state.failed_task_ids)}", flush=True)
        
        while self._running:
            try:
                # Check if user is active — yield if so
                activity = self.scheduler.determine_state()
                if activity == ActivityState.ACTIVE:
                    time.sleep(30)
                    continue
                
                # Check LLM health
                restarted = self.updater.check_and_restart_llms()
                if restarted:
                    print(f"  🔧 Restarted LLMs: {restarted}", flush=True)
                    time.sleep(10)
                
                # Send periodic status report to Telegram
                from .status_reporter import StatusReporter
                reporter = StatusReporter()
                if reporter.should_send(interval_seconds=3600):
                    reporter.send_report()
                
                # Check Docker services (MC, HireFlow backend)
                from .docker_monitor import DockerMonitor
                docker = DockerMonitor()
                docker_restarted = docker.check_and_restart_all()
                if docker_restarted:
                    print(f"  🐳 Docker: {docker_restarted}", flush=True)
                    self.notify.send_message(
                        f"🐳 ForgeFleet restarted Docker: {', '.join(docker_restarted)}",
                        silent=True,
                    )
                
                # Run the current phase
                if self.state.phase == "work":
                    self._phase_work()
                elif self.state.phase == "learn":
                    self._phase_learn()
                elif self.state.phase == "analyze":
                    self._phase_analyze()
                elif self.state.phase == "self_update":
                    self._phase_self_update()
                elif self.state.phase == "verify":
                    self._phase_verify()
                elif self.state.phase == "research":
                    self._phase_research()
                elif self.state.phase == "idle":
                    self._phase_idle()
                
                self.state.save()
                
            except Exception as e:
                self.resilience.log_error(f"Lifecycle phase {self.state.phase}", e)
                print(f"  ❌ Lifecycle error: {e}", flush=True)
                time.sleep(60)
        
        self.state.save()
        print("⛔ Lifecycle stopped", flush=True)
    
    def _phase_work(self):
        """Build tickets."""
        print(f"\n📋 Phase: WORK", flush=True)
        
        if not self.worker:
            self.state.phase = "idle"
            return
        
        from .mc_client import MCClient
        mc = MCClient()
        tickets = mc.get_claimable()
        
        if not tickets:
            print("  No tickets available", flush=True)
            self.state.phase = "research" if self._should_research() else "idle"
            return
        
        # Pick a ticket — skip EPICs that need decomposition first
        ticket = tickets[0]
        title = ticket.get("title", "")
        desc = ticket.get("description", title)
        
        # Detect if this is an EPIC/feature that needs breakdown
        is_epic = any(tag in title.upper() for tag in ["[EPIC]", "[FEATURE]", "[CRITICAL]"])
        is_complex = len(desc) > 500 or any(kw in desc.lower() for kw in ["multiple", "all ", "entire", "complete", "full "])
        
        if is_epic or is_complex:
            print(f"  📋 Detected: {title[:50]} — decomposing...", flush=True)
            
            from .task_decomposer import TaskDecomposer
            from .llm import LLM
            
            # Use 72B for EPIC/FEATURE decomposition (9B is too weak for this)
            decomposer = TaskDecomposer(use_smart_model=True)
            
            # Determine level: EPIC → Features, FEATURE → Tickets, else → Subtasks
            if "[EPIC]" in title.upper():
                level = "features"
                child_prefix = "[FEATURE]"
                prompt = f"Recruitment system: {desc}"  # Keep it simple — let decomposer handle the breakdown
            elif "[FEATURE]" in title.upper() or is_complex:
                level = "tickets"
                child_prefix = ""
                prompt = desc
            else:
                level = "subtasks"
                child_prefix = ""
                prompt = desc
            
            subtasks = decomposer.decompose(prompt)
            
            if len(subtasks) > 1:
                print(f"  → Created {len(subtasks)} {level}", flush=True)
                
                # Create child tickets with DEPENDENCY ordering
                created_titles = []
                prev_ticket_id = None
                
                for i, st in enumerate(subtasks):
                    child_title = f"{child_prefix} {st.title}".strip() if child_prefix else st.title
                    
                    # First ticket is todo, rest are blocked (depend on previous)
                    child_status = "todo" if i == 0 else "blocked"
                    child_desc = st.description
                    if prev_ticket_id and i > 0:
                        child_desc += f"\n\nDepends on: {created_titles[-1] if created_titles else 'previous task'}"
                    
                    result = mc._request("POST", "/api/work-items", {
                        "title": child_title,
                        "description": child_desc,
                        "status": child_status,
                        "priority": "high",
                        "parent_id": ticket["id"],
                    })
                    
                    # Track the created ticket ID for dependency chain
                    if isinstance(result, dict) and "id" in result:
                        prev_ticket_id = result["id"]
                    
                    created_titles.append(child_title)
                
                # Mark parent as in-progress
                mc.update_ticket(ticket["id"], "in_progress",
                                result=f"Decomposed into {len(subtasks)} {level} by ForgeFleet")
                
                self.notify.send_message(
                    f"📋 ForgeFleet decomposed {'EPIC into FEATURES' if level == 'features' else 'FEATURE into TICKETS' if level == 'tickets' else 'task into subtasks'}:\n\n"
                    f"Parent: {title[:50]}\n\n" +
                    "\n".join(f"  {'🏗️' if level == 'features' else '📋'} {t[:55]}" for t in created_titles[:7]) +
                    (f"\n  ... and {len(created_titles)-7} more" if len(created_titles) > 7 else ""),
                )
                return  # Next iteration picks up the children
            
            # If decomposer returned 1 or 0 subtasks, treat as buildable
            print(f"  → Not decomposable, building directly", flush=True)
        
        print(f"  Building: {title[:50]}", flush=True)
        
        self.resilience.log_build(BuildLog(
            timestamp=datetime.now().isoformat(),
            ticket_id=ticket["id"],
            title=ticket["title"],
            status="started",
        ))
        
        # The worker handles the full seniority pipeline
        success = self.worker._work_on_ticket(ticket)
        
        self.state.tasks_since_last_analyze += 1
        
        if not success:
            self.state.failed_task_ids.append(ticket["id"])
        
        # Notify via Telegram
        if success:
            self.notify.send_message(
                f"✅ ForgeFleet built: {ticket['title'][:50]}\n"
                f"Task #{self.state.tasks_since_last_analyze} this cycle",
                silent=True,
            )
        else:
            self.notify.send_message(
                f"❌ ForgeFleet failed: {ticket['title'][:50]}",
                silent=True,
            )
        
        self.resilience.log_build(BuildLog(
            timestamp=datetime.now().isoformat(),
            ticket_id=ticket["id"],
            title=ticket["title"],
            status="completed" if success else "failed",
        ))
        
        # After every task, check if there are branches to review + merge
        self._review_and_merge()
        
        # Transition logic:
        recent_failures = len([fid for fid in self.state.failed_task_ids[-3:]])
        if recent_failures >= 3:
            print(f"  ⚠️ 3+ consecutive failures — triggering analysis + self-update", flush=True)
            self.notify.send_message(
                f"⚠️ ForgeFleet: 3+ failures detected. Analyzing and self-updating...",
                silent=True,
            )
            self.state.phase = "learn"
        elif self.state.tasks_since_last_analyze >= self.ANALYZE_EVERY_N_TASKS:
            self.state.phase = "learn"
        # Otherwise keep working
    
    def _phase_learn(self):
        """Record and process outcomes."""
        print(f"\n📊 Phase: LEARN", flush=True)
        # Evolution engine already records during work phase
        # This is where we consolidate
        stats = self.evolution._overall_success_rate()
        print(f"  Success rate: {stats}", flush=True)
        self.state.phase = "analyze"
    
    def _phase_analyze(self):
        """Find issues and plan improvements. ALWAYS try to self-update if there are unapplied insights."""
        print(f"\n🔍 Phase: ANALYZE", flush=True)
        
        insights = self.evolution.analyze()
        print(f"  Found {len(insights)} insights", flush=True)
        
        for i in insights[:3]:
            print(f"  [{i.category}] {i.finding[:60]}", flush=True)
        
        self.state.tasks_since_last_analyze = 0
        
        # Check for unapplied insights — ALWAYS try to fix them
        unapplied = self.evolution.db.execute(
            "SELECT COUNT(*) FROM insights WHERE applied=0"
        ).fetchone()[0]
        
        if unapplied > 0 and self._should_self_update():
            print(f"  🔧 {unapplied} unapplied insights — triggering self-update", flush=True)
            self.notify.send_message(
                f"🔧 ForgeFleet analyzing {unapplied} unapplied insights...",
                silent=True,
            )
            self.state.phase = "self_update"
        elif self.state.failed_task_ids:
            self.state.phase = "verify"
        else:
            self.state.phase = "work"
    
    def _phase_self_update(self):
        """Stop gracefully, fix itself, restart."""
        print(f"\n🔧 Phase: SELF-UPDATE", flush=True)
        
        # First: apply learnings from errors to prompt templates (quick, no code changes)
        self.updater.apply_error_learnings()
        
        # Then: try deeper code-level fixes
        results = self.updater.run_improvement_cycle()
        
        for r in results:
            if r.deployed:
                print(f"  ✅ Deployed: {r.fix_description[:50]}", flush=True)
                self.state.last_self_update = time.time()
                self.notify.send_message(
                    f"🔧 ForgeFleet SELF-UPDATE deployed!\n\n"
                    f"Problem: {r.insight[:100]}\n"
                    f"Fix: {r.fix_description[:100]}\n"
                    f"Files changed: {r.files_changed}",
                )
            elif r.reverted:
                print(f"  ↩️ Reverted: {r.error[:50]}", flush=True)
                self.notify.send_message(
                    f"↩️ ForgeFleet self-update REVERTED\n"
                    f"Reason: {r.error[:100]}",
                    silent=True,
                )
            else:
                print(f"  ⚠️ No change: {r.error[:50]}", flush=True)
        
        # After update, verify failed tasks
        if self.state.failed_task_ids:
            self.state.phase = "verify"
        else:
            self.state.phase = "work"
    
    def _phase_verify(self):
        """Retry failed tasks to see if the fix worked."""
        print(f"\n✅ Phase: VERIFY ({len(self.state.failed_task_ids)} tasks to retry)", flush=True)
        
        if not self.state.failed_task_ids or not self.worker:
            self.state.phase = "work"
            return
        
        # Retry the first failed task
        from .mc_client import MCClient
        mc = MCClient()
        
        retry_id = self.state.failed_task_ids.pop(0)
        
        # Check if ticket still exists and is retryable
        tickets = mc.get_tickets()
        ticket = next((t for t in tickets if t["id"] == retry_id and t.get("status") == "todo"), None)
        
        if ticket:
            print(f"  Retrying: {ticket['title'][:50]}", flush=True)
            success = self.worker._work_on_ticket(ticket)
            
            if success:
                print(f"  ✅ Retry succeeded — fix worked!", flush=True)
            else:
                print(f"  ❌ Retry failed — fix didn't help", flush=True)
                # Don't re-add to retry list (prevent infinite loop)
        
        self.state.phase = "work"
    
    def _phase_research(self):
        """When idle, study and improve."""
        print(f"\n🔍 Phase: RESEARCH", flush=True)
        
        try:
            # Research competitors
            report = self.researcher.research_competitors()
            print(f"  Competitors: {len(report.findings)} findings", flush=True)
            
            # Research trends
            trends = self.researcher.research_trends()
            print(f"  Trends: {len(trends.findings)} findings", flush=True)
            
            # Run continuous improvement
            cycle = self.improver.run_cycle()
            print(f"  Improvement tickets created: {len(cycle.tickets_created)}", flush=True)
            
            self.state.last_research = time.time()
        except Exception as e:
            print(f"  Research error: {e}", flush=True)
        
        self.state.phase = "work"
        self.state.total_cycles += 1
    
    def _review_and_merge(self):
        """Review ready branches and merge approved ones to main."""
        import subprocess
        
        if not self.worker:
            return
        
        repo = self.worker.repo_dir
        
        # Get ready_for_review tickets with branches
        from .mc_client import MCClient
        mc = MCClient()
        review_tickets = mc.get_tickets(status="ready_for_review")
        
        if not review_tickets:
            return
        
        print(f"  🔍 Reviewing {len(review_tickets)} branches...", flush=True)
        
        # Get the review LLM (72B)
        llm = self.router.get_llm(3)
        if not llm:
            return
        
        merged = 0
        for ticket in review_tickets[:5]:  # Review 5 per cycle
            tid = ticket["id"]
            title = ticket["title"]
            
            # Find branch
            branches = subprocess.run(
                ["git", "branch", "-r"], capture_output=True, text=True, cwd=repo
            ).stdout
            
            branch = None
            for line in branches.split("\n"):
                if tid[:8] in line:
                    branch = line.strip()
                    break
            
            if not branch:
                continue
            
            # Get diff
            diff = subprocess.run(
                ["git", "diff", f"main..{branch}"],
                capture_output=True, text=True, cwd=repo, timeout=10
            ).stdout[:4000]
            
            if not diff.strip():
                continue
            
            # Auto-reject Python in Rust project
            if "from flask" in diff or "import flask" in diff:
                continue
            
            # Quick review with LLM
            try:
                response = llm.call([
                    {"role": "system", "content": "Review this code diff. Say APPROVE or list issues. Be brief."},
                    {"role": "user", "content": f"Ticket: {title}\n\n{diff[:3000]}"},
                ])
                review = response.get("content", "")
                
                if any(w in review.upper() for w in ["APPROVE", "LGTM", "LOOKS GOOD"]):
                    # Merge
                    subprocess.run(["git", "checkout", "main"], capture_output=True, cwd=repo)
                    result = subprocess.run(
                        ["git", "merge", branch, "--no-edit"],
                        capture_output=True, text=True, cwd=repo
                    )
                    
                    if result.returncode == 0:
                        merged += 1
                        mc.update_ticket(tid, "done", result="Reviewed + merged by ForgeFleet")
                        self.notify.send_message(
                            f"✅ Reviewed + merged: {title[:40]}",
                            silent=True,
                        )
                    else:
                        subprocess.run(["git", "merge", "--abort"], capture_output=True, cwd=repo)
            except Exception:
                pass
        
        # Push merged main
        if merged > 0:
            subprocess.run(["git", "push", "origin", "main"], capture_output=True, cwd=repo)
            print(f"  ✅ Merged {merged} branches to main", flush=True)
    
    def _phase_idle(self):
        """Nothing to do — wait and check periodically."""
        if self._should_research():
            self.state.phase = "research"
        elif self._should_self_update():
            self.state.phase = "self_update"
        else:
            time.sleep(60)  # Check again in 1 minute
    
    def _should_research(self) -> bool:
        return time.time() - self.state.last_research > self.RESEARCH_INTERVAL
    
    def _should_self_update(self) -> bool:
        return time.time() - self.state.last_self_update > self.SELF_UPDATE_INTERVAL
    
    def _shutdown(self):
        """Graceful shutdown — save state."""
        print("\n⛔ Lifecycle shutting down gracefully...", flush=True)
        self._running = False
        self.state.save()
    
    def status(self) -> dict:
        return {
            "phase": self.state.phase,
            "total_cycles": self.state.total_cycles,
            "tasks_since_analyze": self.state.tasks_since_last_analyze,
            "failed_queue": len(self.state.failed_task_ids),
            "last_self_update": datetime.fromtimestamp(self.state.last_self_update).isoformat() if self.state.last_self_update else "never",
            "last_research": datetime.fromtimestamp(self.state.last_research).isoformat() if self.state.last_research else "never",
        }


if __name__ == "__main__":
    repo = sys.argv[1] if len(sys.argv) > 1 else ""
    manager = LifecycleManager(repo_dir=repo)
    manager.run()
