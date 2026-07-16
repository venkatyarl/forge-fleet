"""Multi-Turn Conversation — iterative back-and-forth with agents.

Item #4: Instead of one-shot, let agents refine:
"write auth" → "now make it async" → "add error handling" → "add tests"
"""
from dataclasses import dataclass, field
from .agent import Agent
from .llm import LLM


@dataclass  
class ConversationTurn:
    """One turn in a multi-turn conversation."""
    role: str  # "user" (task giver) or "agent"
    content: str
    files_changed: list = field(default_factory=list)


class AgentConversation:
    """Multi-turn conversation with an agent.
    
    Maintains message history so the agent remembers context
    across multiple refinement requests.
    """
    
    def __init__(self, agent: Agent):
        self.agent = agent
        self.history: list[ConversationTurn] = []
        self._messages: list[dict] = [
            {"role": "system", "content": agent._build_system_prompt()},
        ]
    
    def say(self, message: str) -> str:
        """Send a message and get a response, maintaining context."""
        self.history.append(ConversationTurn(role="user", content=message))
        self._messages.append({"role": "user", "content": message})
        
        # Build tool schemas
        tool_schemas = [t.to_openai_schema() for t in self.agent.tools] if self.agent.tools else None
        tool_map = {t.name: t for t in self.agent.tools} if self.agent.tools else {}
        
        # Agent loop with tool calling
        for _ in range(self.agent.max_iterations):
            try:
                response = self.agent.llm.call(self._messages, tools=tool_schemas)
            except RuntimeError as e:
                error_msg = f"LLM error: {e}"
                self.history.append(ConversationTurn(role="agent", content=error_msg))
                return error_msg
            
            tool_calls = response.get("tool_calls", [])
            
            if tool_calls:
                self._messages.append(response)
                for tc in tool_calls:
                    import json
                    func_name = tc["function"]["name"]
                    try:
                        args = json.loads(tc["function"]["arguments"])
                    except (json.JSONDecodeError, KeyError):
                        args = {}
                    
                    tool = tool_map.get(func_name)
                    result = tool.run(**args) if tool else f"Unknown tool: {func_name}"
                    
                    self._messages.append({
                        "role": "tool",
                        "tool_call_id": tc.get("id", f"call_{func_name}"),
                        "content": result[:8000],
                    })
                continue
            
            content = response.get("content", "")
            if content:
                self._messages.append({"role": "assistant", "content": content})
                self.history.append(ConversationTurn(role="agent", content=content))
                return content
        
        return "Max iterations reached"
    
    def undo(self) -> str:
        """Undo the last exchange (remove last user + agent turns)."""
        if len(self._messages) >= 3:  # Keep at least system prompt
            # Remove last assistant + user messages
            while self._messages and self._messages[-1]["role"] != "system":
                removed = self._messages.pop()
                if removed["role"] == "user":
                    break
            
            if self.history:
                self.history.pop()  # Remove agent turn
            if self.history:
                self.history.pop()  # Remove user turn
            
            return "Last exchange undone"
        return "Nothing to undo"
    
    def summary(self) -> str:
        """Get a summary of the conversation so far."""
        lines = [f"Conversation with {self.agent.role} ({len(self.history)} turns):"]
        for turn in self.history[-6:]:  # Last 6 turns
            prefix = "👤" if turn.role == "user" else "🤖"
            lines.append(f"  {prefix} {turn.content[:100]}")
        return "\n".join(lines)
    
    def context_size(self) -> int:
        """Estimate current context size in characters."""
        return sum(len(m.get("content", "")) for m in self._messages)
