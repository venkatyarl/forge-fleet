"""Crew — orchestrates multiple agents executing tasks in sequence.

Core pattern from CrewAI's Crew class, simplified to the essential:
- Sequential process: tasks execute one after another
- Each task's output becomes context for the next
- Results are collected and returned
"""
import time
from dataclasses import dataclass, field
from .task import Task


@dataclass
class Crew:
    """A crew of agents working together on a sequence of tasks.
    
    Process types:
    - sequential: tasks run in order, each getting context from previous
    - (hierarchical and custom can be added later)
    """
    agents: list = field(default_factory=list)
    tasks: list = field(default_factory=list)
    verbose: bool = True
    
    def kickoff(self) -> dict:
        """Execute all tasks sequentially and return results."""
        start = time.time()
        results = []
        
        if self.verbose:
            print(f"\n🚀 Crew starting — {len(self.agents)} agents, {len(self.tasks)} tasks")
        
        for i, task in enumerate(self.tasks):
            if self.verbose:
                agent_name = task.agent.role if task.agent else "unassigned"
                print(f"\n📋 Task {i+1}/{len(self.tasks)}: {agent_name}")
                desc_preview = task.description.replace('\n', ' ')[:80]
                print(f"   {desc_preview}...")
            
            task_start = time.time()
            
            try:
                output = task.execute()
                task_time = time.time() - task_start
                
                results.append({
                    "task": i + 1,
                    "agent": task.agent.role if task.agent else "none",
                    "output": output,
                    "time": round(task_time, 1),
                    "success": True,
                })
                
                if self.verbose:
                    print(f"   ✅ Done in {task_time:.1f}s ({len(output)} chars)")
                    
            except Exception as e:
                task_time = time.time() - task_start
                error_msg = str(e)
                
                results.append({
                    "task": i + 1,
                    "agent": task.agent.role if task.agent else "none",
                    "output": error_msg,
                    "time": round(task_time, 1),
                    "success": False,
                })
                
                if self.verbose:
                    print(f"   ❌ Failed in {task_time:.1f}s: {error_msg[:100]}")
        
        total_time = time.time() - start
        
        return {
            "results": results,
            "total_time": round(total_time, 1),
            "final_output": results[-1]["output"] if results else "",
            "success": all(r["success"] for r in results),
        }
