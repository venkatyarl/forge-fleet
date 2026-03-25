"""Tool — base class for tools that agents can use."""
from dataclasses import dataclass, field
from typing import Callable


@dataclass
class Tool:
    """A tool that an agent can invoke.
    
    Pattern extracted from CrewAI's BaseTool, simplified:
    - name: unique identifier
    - description: what the tool does (shown to LLM)
    - parameters: JSON schema for arguments
    - func: the actual function to call
    """
    name: str
    description: str
    parameters: dict = field(default_factory=lambda: {"type": "object", "properties": {}})
    func: Callable[..., str] = None
    
    def run(self, **kwargs) -> str:
        """Execute the tool with given arguments."""
        if self.func is None:
            return f"Tool '{self.name}' has no implementation"
        try:
            return str(self.func(**kwargs))
        except Exception as e:
            return f"Tool error ({self.name}): {e}"
    
    def to_openai_schema(self) -> dict:
        """Convert to OpenAI function calling format."""
        return {
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        }
