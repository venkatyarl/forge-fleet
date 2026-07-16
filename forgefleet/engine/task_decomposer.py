"""Task Decomposer — break complex tasks into manageable subtasks.

The #1 improvement for local LLM quality.
"Build auth system" → 5 small tasks each achievable by a 32B model.
"""
import json
from dataclasses import dataclass, field
from .llm import LLM


@dataclass
class Subtask:
    """A decomposed subtask."""
    index: int
    title: str
    description: str
    depends_on: list = field(default_factory=list)  # indices of subtasks this depends on
    estimated_tier: int = 2  # suggested model tier
    files_involved: list = field(default_factory=list)


class TaskDecomposer:
    """Break complex tasks into subtasks using an LLM.
    
    Uses a small fast model (9B) for decomposition since it's
    a reasoning task, not a coding task.
    """
    
    def __init__(self, llm: LLM = None, use_smart_model: bool = False):
        if llm:
            self.llm = llm
        elif use_smart_model:
            # Use 72B for better decomposition
            self.llm = LLM(base_url="http://192.168.5.100:51801/v1", model="qwen2.5-72b", timeout=300)
        else:
            self.llm = LLM(base_url="http://192.168.5.100:51803/v1")
    
    def decompose(self, task: str, max_subtasks: int = 7) -> list[Subtask]:
        """Break a task into subtasks."""
        messages = [
            {"role": "system", "content": (
                "You are a senior tech lead who breaks down coding tasks into small, "
                "independent subtasks. Each subtask should be completable by a single "
                "developer in under 30 minutes.\n\n"
                "Output ONLY valid JSON array. Each item has:\n"
                '{"title": "short title", "description": "what to do", '
                '"depends_on": [indices], "tier": 1-3, "files": ["file paths"]}\n\n'
                "tier 1 = simple (struct, config), tier 2 = moderate (logic, handlers), "
                "tier 3 = complex (async, security, multi-file)\n\n"
                "Output ONLY the JSON array, nothing else."
            )},
            {"role": "user", "content": f"Break this task into {max_subtasks} or fewer subtasks:\n\n{task}"},
        ]
        
        try:
            response = self.llm.call(messages)
            content = response.get("content", "")
            
            # Extract JSON from response
            subtasks = self._parse_json(content)
            
            return [
                Subtask(
                    index=i,
                    title=s.get("title", f"Subtask {i+1}"),
                    description=s.get("description", ""),
                    depends_on=s.get("depends_on", []),
                    estimated_tier=s.get("tier", 2),
                    files_involved=s.get("files", []),
                )
                for i, s in enumerate(subtasks[:max_subtasks])
            ]
        except Exception:
            # Fallback: return the original task as a single subtask
            return [Subtask(index=0, title=task[:60], description=task)]
    
    def _parse_json(self, text: str) -> list:
        """Extract JSON array from LLM response."""
        # Try direct parse
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            pass
        
        # Try extracting from code block
        import re
        m = re.search(r'```(?:json)?\s*\n?(.*?)```', text, re.DOTALL)
        if m:
            try:
                return json.loads(m.group(1))
            except json.JSONDecodeError:
                pass
        
        # Try finding array brackets
        start = text.find('[')
        end = text.rfind(']')
        if start >= 0 and end > start:
            try:
                return json.loads(text[start:end+1])
            except json.JSONDecodeError:
                pass
        
        return []
    
    def execution_order(self, subtasks: list[Subtask]) -> list[list[Subtask]]:
        """Determine execution order respecting dependencies.
        
        Returns stages: [[independent tasks], [next batch], ...]
        Tasks in the same stage can run in parallel.
        """
        completed = set()
        stages = []
        remaining = list(subtasks)
        
        while remaining:
            stage = []
            for task in remaining:
                deps_met = all(d in completed for d in task.depends_on)
                if deps_met:
                    stage.append(task)
            
            if not stage:
                # Circular dependency or error — add remaining as last stage
                stages.append(remaining)
                break
            
            stages.append(stage)
            for t in stage:
                completed.add(t.index)
                remaining.remove(t)
        
        return stages
