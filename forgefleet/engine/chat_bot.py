"""Chat Bot Integration — fleet control from Slack/Discord/Telegram.

Item #13: Control ForgeFleet from any chat platform.
Parses natural language commands and routes to the right engine function.
"""
import re
from dataclasses import dataclass, field
from typing import Callable


@dataclass
class ChatCommand:
    """A parsed command from chat."""
    action: str  # "status", "scan", "run", "tickets", "benchmark", "costs"
    args: dict = field(default_factory=dict)
    raw: str = ""


class FleetChatBot:
    """Parse chat messages into fleet commands.
    
    Examples:
    "fleet status" → status()
    "scan network" → scan()
    "run auth task on 32B" → run(task, tier=2)
    "show tickets" → tickets()
    "what's the cost savings" → costs()
    "benchmark 9B vs 32B on code" → benchmark(9B, 32B, code)
    """
    
    PATTERNS = [
        (r"(?:fleet\s+)?status", "status", {}),
        (r"scan\s*(?:network|fleet|subnet)?", "scan", {}),
        (r"show\s*tickets?|ticket\s*stats?", "tickets", {}),
        (r"cost\s*(?:savings?|report|tracker)?|how\s*much\s*saved", "costs", {}),
        (r"benchmark\s+(.+)\s+(?:vs|versus)\s+(.+)", "benchmark", {"models": True}),
        (r"run\s+(.+?)(?:\s+on\s+(\w+))?$", "run", {"task": True}),
        (r"who\s*(?:is|are)\s*(?:busy|working)", "busy", {}),
        (r"(?:stop|kill)\s+(.+)", "stop", {"target": True}),
        (r"logs?\s+(\w+)", "logs", {"node": True}),
        (r"events?|what\s*(?:'s)?\s*happen", "events", {}),
        (r"help|what\s+can\s+you\s+do", "help", {}),
    ]
    
    def parse(self, message: str) -> ChatCommand:
        """Parse a chat message into a command."""
        msg = message.strip().lower()
        
        for pattern, action, meta in self.PATTERNS:
            m = re.search(pattern, msg, re.IGNORECASE)
            if m:
                args = {}
                groups = m.groups()
                
                if meta.get("models") and len(groups) >= 2:
                    args["model_a"] = groups[0].strip()
                    args["model_b"] = groups[1].strip()
                elif meta.get("task") and groups:
                    args["task"] = groups[0].strip()
                    if len(groups) > 1 and groups[1]:
                        args["node"] = groups[1].strip()
                elif meta.get("target") and groups:
                    args["target"] = groups[0].strip()
                elif meta.get("node") and groups:
                    args["node"] = groups[0].strip()
                
                return ChatCommand(action=action, args=args, raw=message)
        
        return ChatCommand(action="unknown", raw=message)
    
    def help_text(self) -> str:
        """Return help text showing available commands."""
        return """⚡ ForgeFleet Commands:

• **fleet status** — show fleet health + model status
• **scan network** — discover new LLM endpoints
• **show tickets** — ticket statistics from MC
• **cost savings** — how much we've saved vs API
• **benchmark 9B vs 32B on code** — A/B test models
• **run [task] on [node]** — execute a task
• **who is busy** — show busy endpoints
• **events** — recent fleet events
• **logs [node]** — show node logs
• **stop [task]** — stop a running task"""
