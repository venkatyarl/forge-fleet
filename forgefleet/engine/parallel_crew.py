"""Parallel Crew — run agents on different nodes simultaneously.

Item #4: Instead of sequential (A finishes → B starts),
run Context Engineer on Node A while Code Writer prepares on Node B.
Partial context streaming between agents.
"""
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from .agent import Agent
from .task import Task
from .fleet_router import FleetRouter


@dataclass
class ParallelCrew:
    """Run agents in parallel across fleet nodes.
    
    Modes:
    - sequential: A → B → C (original)
    - parallel_independent: A | B | C (all independent tasks)
    - pipeline: A → (B | C) → D (some parallel, some sequential)
    """
    agents: list = field(default_factory=list)
    tasks: list = field(default_factory=list)
    router: FleetRouter = field(default_factory=FleetRouter)
    verbose: bool = True
    
    def kickoff_parallel(self, max_workers: int = 4) -> dict:
        """Execute independent tasks in parallel across nodes."""
        start = time.time()
        results = []
        
        if self.verbose:
            print(f"\n🚀 Parallel Crew — {len(self.tasks)} tasks across fleet")
        
        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            futures = {}
            for i, task in enumerate(self.tasks):
                future = executor.submit(self._execute_task, i, task)
                futures[future] = i
            
            for future in as_completed(futures):
                idx = futures[future]
                try:
                    result = future.result()
                    results.append(result)
                    if self.verbose:
                        icon = "✅" if result["success"] else "❌"
                        print(f"  {icon} Task {idx+1}: {result['agent']} ({result['time']}s)")
                except Exception as e:
                    results.append({"task": idx, "success": False, "error": str(e)})
        
        results.sort(key=lambda r: r.get("task", 0))
        
        return {
            "results": results,
            "total_time": round(time.time() - start, 1),
            "success": all(r.get("success") for r in results),
            "mode": "parallel",
        }
    
    def kickoff_pipeline(self, stages: list[list[int]] = None) -> dict:
        """Execute tasks in pipeline stages.
        
        stages = [[0], [1, 2], [3]] means:
        - Stage 1: task 0 runs alone
        - Stage 2: tasks 1 and 2 run in parallel
        - Stage 3: task 3 runs alone (with context from 1+2)
        """
        if stages is None:
            # Default: first task alone, rest in parallel, then review
            if len(self.tasks) <= 2:
                stages = [[i] for i in range(len(self.tasks))]
            else:
                stages = [[0], list(range(1, len(self.tasks)-1)), [len(self.tasks)-1]]
        
        start = time.time()
        all_results = []
        
        for stage_num, task_indices in enumerate(stages):
            if self.verbose:
                print(f"\n📋 Stage {stage_num+1}: {len(task_indices)} task(s)")
            
            if len(task_indices) == 1:
                # Sequential
                result = self._execute_task(task_indices[0], self.tasks[task_indices[0]])
                all_results.append(result)
            else:
                # Parallel
                with ThreadPoolExecutor(max_workers=len(task_indices)) as executor:
                    futures = {}
                    for idx in task_indices:
                        future = executor.submit(self._execute_task, idx, self.tasks[idx])
                        futures[future] = idx
                    
                    for future in as_completed(futures):
                        result = future.result()
                        all_results.append(result)
        
        all_results.sort(key=lambda r: r.get("task", 0))
        
        return {
            "results": all_results,
            "total_time": round(time.time() - start, 1),
            "success": all(r.get("success") for r in all_results),
            "mode": "pipeline",
            "stages": len(stages),
        }
    
    def _execute_task(self, idx: int, task: Task) -> dict:
        """Execute a single task."""
        task_start = time.time()
        try:
            output = task.execute()
            return {
                "task": idx,
                "agent": task.agent.role if task.agent else "none",
                "output": output,
                "time": round(time.time() - task_start, 1),
                "success": True,
            }
        except Exception as e:
            return {
                "task": idx,
                "agent": task.agent.role if task.agent else "none",
                "output": str(e),
                "time": round(time.time() - task_start, 1),
                "success": False,
            }
