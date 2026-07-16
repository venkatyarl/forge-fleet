"""Streaming Progress — real-time updates during long-running tasks.

Item #5: Instead of blocking for minutes, stream progress back to MCP clients.
Uses a simple event queue that MCP server can poll.
"""
import time
import threading
from dataclasses import dataclass, field
from collections import deque


@dataclass
class ProgressEvent:
    """A progress update from a running task."""
    task_id: str
    agent: str
    status: str  # "started", "tool_call", "result", "complete", "error"
    message: str
    timestamp: float = 0
    
    def __post_init__(self):
        if not self.timestamp:
            self.timestamp = time.time()


class ProgressStream:
    """Thread-safe progress event stream.
    
    Agents push events, MCP server polls for new events.
    Keeps last 100 events per task.
    """
    
    def __init__(self, max_events: int = 100):
        self.max_events = max_events
        self._events: deque = deque(maxlen=1000)
        self._lock = threading.Lock()
        self._task_events: dict[str, deque] = {}
    
    def push(self, event: ProgressEvent):
        """Push a new progress event."""
        with self._lock:
            self._events.append(event)
            if event.task_id not in self._task_events:
                self._task_events[event.task_id] = deque(maxlen=self.max_events)
            self._task_events[event.task_id].append(event)
    
    def poll(self, since: float = 0) -> list[ProgressEvent]:
        """Get all events since a timestamp."""
        with self._lock:
            return [e for e in self._events if e.timestamp > since]
    
    def poll_task(self, task_id: str, since: float = 0) -> list[ProgressEvent]:
        """Get events for a specific task."""
        with self._lock:
            events = self._task_events.get(task_id, deque())
            return [e for e in events if e.timestamp > since]
    
    def latest(self, count: int = 10) -> list[ProgressEvent]:
        """Get the N most recent events."""
        with self._lock:
            return list(self._events)[-count:]
    
    def format_latest(self, count: int = 10) -> str:
        """Get formatted latest events for display."""
        events = self.latest(count)
        lines = []
        icons = {
            "started": "🚀", "tool_call": "🔧", "result": "📋",
            "complete": "✅", "error": "❌",
        }
        for e in events:
            icon = icons.get(e.status, "📋")
            age = time.time() - e.timestamp
            ago = f"{int(age)}s ago" if age < 60 else f"{int(age/60)}m ago"
            lines.append(f"{icon} [{e.agent}] {e.message} ({ago})")
        return "\n".join(lines) if lines else "No recent events"


# Global progress stream
_global_stream = ProgressStream()

def get_stream() -> ProgressStream:
    return _global_stream
