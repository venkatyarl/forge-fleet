"""OpenClaw Bridge — integrate ForgeFleet with OpenClaw for notifications.

Sends messages via OpenClaw's gateway API:
- Telegram notifications when tasks complete/fail
- Fleet status updates
- Alert on node failures
"""
import json
import os
import urllib.request
import urllib.error
from dataclasses import dataclass


@dataclass
class OpenClawBridge:
    def __post_init__(self):
        if not self.chat_id:
            from forgefleet import config
            self.chat_id = config.get_telegram_config().get("chat_id", "")
    """Send notifications and messages through OpenClaw's gateway.
    
    Uses the gateway's internal API to send Telegram messages
    without needing a separate bot token.
    """
    gateway_url: str = "http://localhost:50000"
    chat_id: str = ""
    channel: str = "telegram"
    
    def send_message(self, text: str, silent: bool = False) -> bool:
        """Send a message via OpenClaw CLI."""
        import subprocess
        try:
            cmd = [
                "openclaw", "message", "send",
                "--target", self.chat_id,
                "--channel", self.channel,
                "--message", text,
            ]
            if silent:
                cmd.append("--silent")
            
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=15)
            return r.returncode == 0
        except Exception:
            return False
    
    def notify_task_complete(self, task_title: str, branch: str, 
                             agent: str, duration: float):
        """Send notification that a task completed."""
        self.send_message(
            f"✅ ForgeFleet Task Complete\n\n"
            f"Task: {task_title}\n"
            f"Branch: {branch}\n"
            f"Agent: {agent}\n"
            f"Duration: {duration:.1f}s"
        )
    
    def notify_task_failed(self, task_title: str, error: str, tier: int):
        """Send notification that a task failed."""
        self.send_message(
            f"❌ ForgeFleet Task Failed\n\n"
            f"Task: {task_title}\n"
            f"Tier: {tier}\n"
            f"Error: {error[:200]}"
        )
    
    def notify_node_event(self, event_type: str, node: str, details: str):
        """Send notification about fleet node events."""
        icons = {
            "node_joined": "🟢",
            "node_left": "🔴", 
            "node_failed": "❌",
            "node_recovered": "✅",
            "cluster_degraded": "⚠️",
        }
        icon = icons.get(event_type, "📋")
        self.send_message(f"{icon} Fleet: {details}", silent=True)
    
    def notify_scheduler_state(self, old_state: str, new_state: str):
        """Send notification about scheduler state changes."""
        if new_state == "night":
            self.send_message("🌙 ForgeFleet entering night mode — full autonomous work starting", silent=True)
        elif new_state == "active" and old_state != "active":
            self.send_message("👤 Welcome back — ForgeFleet yielding resources", silent=True)
