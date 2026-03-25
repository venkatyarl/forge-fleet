"""Task Router — reads tickets from MC, dispatches to tiered pipeline."""
import json
import subprocess
import os
import time
from typing import Optional
from forgefleet.orchestrator.fleet_discovery import FleetDiscovery
from forgefleet.orchestrator.pipeline import TieredPipeline
from forgefleet.memory.store import MemoryStore, MemoryEntry
from forgefleet.context.repo_context import RepoContext


class TaskRouter:
    """Routes tasks from Mission Control to the tiered pipeline.
    
    Flow:
    1. Poll MC for 'todo' tickets
    2. Check shared memory for similar past tasks
    3. Build context from repo
    4. Run tiered pipeline
    5. Store result in memory
    6. Update MC ticket status
    """
    
    def __init__(self, fleet: FleetDiscovery, memory: MemoryStore, 
                 repo_dir: str, mc_url: str = "http://192.168.5.100:60002",
                 node_name: str = ""):
        self.fleet = fleet
        self.memory = memory
        self.repo_dir = repo_dir
        self.mc_url = mc_url
        self.node_name = node_name or os.uname().nodename
        self.context = RepoContext(repo_dir)
    
    def poll_and_execute(self, max_tasks: int = 1) -> list[dict]:
        """Poll MC for tasks, execute via pipeline, return results."""
        results = []
        
        for _ in range(max_tasks):
            # 1. Get a claimable task
            task = self._claim_task()
            if not task:
                break
            
            task_id = task["id"]
            title = task.get("title", "")
            desc = task.get("description", "")
            
            print(f"📋 Claimed: {title[:60]}")
            
            # 2. Check memory for similar past tasks
            memories = self.memory.recall(title)
            memory_hint = ""
            if memories:
                successes = [m for m in memories if m.outcome == "success"]
                failures = [m for m in memories if m.outcome == "failure"]
                if successes:
                    best = successes[0]
                    memory_hint += f"\nPrevious success on similar task using {best.model_used} (tier {best.tier_completed})"
                if failures:
                    errors = set(m.error_pattern for m in failures if m.error_pattern)
                    if errors:
                        memory_hint += f"\nKnown failure patterns to avoid: {'; '.join(list(errors)[:3])}"
            
            # 3. Build context
            code_context = self.context.get_context(title + " " + desc)
            architecture = self.context.get_architecture()
            
            # 4. Build enhanced description
            full_desc = desc
            if architecture:
                full_desc += f"\n\n## Architecture\n{architecture[:2000]}"
            if code_context:
                full_desc += f"\n\n## Existing Code\n{code_context[:3000]}"
            if memory_hint:
                full_desc += f"\n\n## From Memory{memory_hint}"
            
            # 5. Run pipeline
            branch = f"feat/{self.node_name.lower()}-{task_id[:8]}"
            pipeline = TieredPipeline(self.fleet, self.repo_dir, self.node_name)
            
            result = pipeline.run(
                task_title=title,
                task_description=full_desc,
                branch_name=branch,
            )
            
            # 6. Store in memory
            self.memory.remember(MemoryEntry(
                task_pattern=title.lower(),
                outcome="success" if result["success"] else "failure",
                model_used=f"tier{result['completed_tier']}",
                tier_completed=result["completed_tier"],
                error_pattern=result.get("result", "")[:200] if not result["success"] else "",
                code_pattern="",
                node=self.node_name,
                timestamp=time.time(),
            ))
            
            # 7. Update MC
            if result["success"]:
                self._complete_task(task_id, result["result"][:2000])
            else:
                self._fail_task(task_id, result["result"][:500])
            
            results.append(result)
        
        return results
    
    def _claim_task(self) -> Optional[dict]:
        """Claim a todo task from MC."""
        try:
            r = subprocess.run(
                ["curl", "-s", "--max-time", "10", f"{self.mc_url}/api/work-items"],
                capture_output=True, text=True, timeout=15
            )
            if r.returncode != 0:
                return None
            
            items = json.loads(r.stdout)
            claimable = [i for i in items if i.get("status") in ("todo", "queued")]
            
            if not claimable:
                return None
            
            # Sort by priority
            priority_order = {"critical": 0, "high": 1, "medium": 2, "low": 3}
            claimable.sort(key=lambda x: priority_order.get(x.get("priority", "medium"), 2))
            
            task = claimable[0]
            
            # Claim it
            subprocess.run(
                ["curl", "-s", "-X", "POST", f"{self.mc_url}/api/work-items/{task['id']}/claim",
                 "-H", "Content-Type: application/json",
                 "-d", json.dumps({"node_id": self.node_name})],
                capture_output=True, text=True, timeout=10
            )
            
            return task
        except:
            return None
    
    def _complete_task(self, task_id: str, result: str):
        """Mark task as done in MC."""
        try:
            subprocess.run(
                ["curl", "-s", "-X", "PUT", f"{self.mc_url}/api/work-items/{task_id}",
                 "-H", "Content-Type: application/json",
                 "-d", json.dumps({
                     "status": "done",
                     "result": result,
                     "builder_node_id": self.node_name,
                     "review_checklist": {"auto_review": "pending_qa"},
                 })],
                capture_output=True, text=True, timeout=10
            )
        except:
            pass
    
    def _fail_task(self, task_id: str, reason: str):
        """Mark task as failed in MC."""
        try:
            subprocess.run(
                ["curl", "-s", "-X", "PUT", f"{self.mc_url}/api/work-items/{task_id}",
                 "-H", "Content-Type: application/json",
                 "-d", json.dumps({
                     "status": "todo",
                     "error_log": reason,
                     "assigned_node_id": None,
                 })],
                capture_output=True, text=True, timeout=10
            )
        except:
            pass
