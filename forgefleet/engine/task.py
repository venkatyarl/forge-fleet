"""Task — a unit of work assigned to an agent."""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .lifecycle_policy import LifecyclePolicy


@dataclass
class Task:
    """A task to be executed by an agent.
    
    Pattern from CrewAI: tasks have descriptions, expected outputs,
    assigned agents, and can depend on other tasks' outputs.
    """
    description: str
    expected_output: str = ""
    agent: object = None  # Agent instance
    context_tasks: list = field(default_factory=list)  # Tasks whose output feeds into this
    output: str = ""  # Filled after execution
    retries: int = 0
    review_loops: int = 0
    lifecycle_state: str = "todo"
    
    def execute(self, extra_context: str = "") -> str:
        """Execute the task using its assigned agent."""
        if self.agent is None:
            raise ValueError("Task has no assigned agent")
        
        # Build context from dependent tasks
        context_parts = []
        for ctx_task in self.context_tasks:
            if ctx_task.output:
                agent_name = ctx_task.agent.role if ctx_task.agent else "previous task"
                context_parts.append(
                    f"### Output from {agent_name}:\n{ctx_task.output}"
                )
        
        if extra_context:
            context_parts.append(extra_context)
        
        context = "\n\n".join(context_parts)
        
        # Execute via agent
        self.output = self.agent.execute(self.description, context)
        return self.output
