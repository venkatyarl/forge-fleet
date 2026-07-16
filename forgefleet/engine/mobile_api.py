"""Mobile API — check fleet from phone via simple REST endpoints.

Item #12: Mobile-friendly API that the dashboard or a future app can use.
Serves on the same port as dashboard (51820) with JSON endpoints.
Also accessible via Telegram (OpenClaw bridge).
"""
import json
from dataclasses import dataclass


@dataclass
class MobileAPI:
    """Lightweight API for mobile/remote fleet access.
    
    Endpoints (served by dashboard.py):
    - GET /api/status — full fleet status
    - GET /api/tickets — ticket summary
    - GET /api/events — recent events
    - GET /api/costs — cost savings report
    
    Also works via Telegram: ask Taylor "fleet status" and 
    OpenClawBridge formats it for mobile display.
    """
    
    def format_for_mobile(self, status: dict) -> str:
        """Format fleet status for small screens (Telegram/phone)."""
        lines = ["⚡ ForgeFleet"]
        
        # Endpoints summary
        eps = status.get("endpoints", [])
        healthy = sum(1 for e in eps if e.get("healthy"))
        busy = sum(1 for e in eps if e.get("busy"))
        lines.append(f"🖥️ {healthy}/{len(eps)} nodes up" + (f" ({busy} busy)" if busy else ""))
        
        # By tier
        tiers = {}
        for ep in eps:
            t = ep.get("tier", 0)
            tiers.setdefault(t, {"up": 0, "total": 0})
            tiers[t]["total"] += 1
            if ep.get("healthy"):
                tiers[t]["up"] += 1
        
        tier_names = {1: "9B", 2: "32B", 3: "72B", 4: "235B"}
        for t in sorted(tiers.keys()):
            if t > 0:
                lines.append(f"  T{t} ({tier_names.get(t, '?')}): {tiers[t]['up']}/{tiers[t]['total']}")
        
        # Tickets
        tickets = status.get("tickets", {})
        if tickets:
            lines.append(f"\n📊 Tickets: {tickets.get('done', 0)} done / {tickets.get('total', 0)} total")
            if tickets.get("claimable"):
                lines.append(f"  {tickets['claimable']} ready to claim")
        
        # Recent events
        events = status.get("events", [])
        if events:
            lines.append(f"\n📋 Latest:")
            for e in events[-3:]:
                lines.append(f"  {e.get('icon', '•')} {e.get('message', '')[:50]}")
        
        # Costs
        costs = status.get("costs", {})
        if costs:
            lines.append(f"\n💰 Saved: {costs.get('savings', '$0')}")
        
        return "\n".join(lines)
    
    def format_tickets_mobile(self, stats: dict) -> str:
        """Format ticket stats for mobile."""
        lines = ["📋 Tickets"]
        for status, count in stats.get("by_status", {}).items():
            icons = {"done": "✅", "todo": "📝", "blocked": "🚫", 
                     "in_progress": "🔄", "ready_for_review": "👀"}
            lines.append(f"  {icons.get(status, '•')} {status}: {count}")
        return "\n".join(lines)
