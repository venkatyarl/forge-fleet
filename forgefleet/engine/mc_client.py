"""Mission Control Client — API client for the MC ticket system.

Connects ForgeFleet to Mission Control for autonomous work:
- Claim tickets from the backlog
- Update ticket status
- Create review tickets
- Get project backlog stats
"""
import json
import urllib.request
import urllib.error
from dataclasses import dataclass, field
from typing import Optional


@dataclass
class MCClient:
    def __post_init__(self):
        if not self.base_url:
            from forgefleet import config
            self.base_url = config.get_mc_url()
    """Client for the Mission Control API.
    
    MC runs on Taylor at port 60002.
    """
    base_url: str = ""
    node_name: str = ""
    timeout: int = 15
    
    def _request(self, method: str, path: str, data: dict = None) -> dict:
        """Make an API request to MC."""
        url = f"{self.base_url}{path}"
        
        req = urllib.request.Request(url, method=method)
        req.add_header("Content-Type", "application/json")
        
        body = json.dumps(data).encode() if data else None
        
        try:
            with urllib.request.urlopen(req, body, timeout=self.timeout) as resp:
                return json.loads(resp.read())
        except urllib.error.HTTPError as e:
            error_body = e.read().decode() if e.fp else ""
            return {"error": f"HTTP {e.code}: {error_body[:200]}"}
        except Exception as e:
            return {"error": str(e)}
    
    def health(self) -> bool:
        """Check if MC is healthy."""
        try:
            result = self._request("GET", "/health")
            return result.get("status") == "ok"
        except Exception:
            return False
    
    def get_tickets(self, status: str = None, project: str = None,
                    limit: int = 50) -> list[dict]:
        """Get tickets, optionally filtered by status and project."""
        result = self._request("GET", "/api/work-items")
        if isinstance(result, list):
            tickets = result
        elif isinstance(result, dict) and "error" not in result:
            tickets = result.get("items", result.get("data", []))
        else:
            return []
        
        # Filter
        if status:
            tickets = [t for t in tickets if t.get("status") == status]
        if project:
            tickets = [t for t in tickets if project.lower() in t.get("title", "").lower()]
        
        return tickets[:limit]
    
    def get_claimable(self, project: str = None) -> list[dict]:
        """Get tickets that can be claimed (todo/queued status)."""
        tickets = self.get_tickets()
        claimable = [t for t in tickets if t.get("status") in ("todo", "queued")]
        
        if project:
            claimable = [t for t in claimable if project.lower() in t.get("title", "").lower()]
        
        # Sort by priority
        priority_order = {"critical": 0, "high": 1, "medium": 2, "low": 3}
        claimable.sort(key=lambda t: priority_order.get(t.get("priority", "medium"), 2))
        
        return claimable
    
    def claim_ticket(self, ticket_id: str) -> dict:
        """Claim a ticket for this node."""
        return self._request("POST", f"/api/work-items/{ticket_id}/claim", {
            "node_id": self.node_name,
        })
    
    def update_ticket(self, ticket_id: str, status: str, result: str = "",
                      error: str = "", branch: str = "") -> dict:
        """Update a ticket's status."""
        data = {"status": status}
        if result:
            data["result"] = result
        if error:
            data["error_log"] = error
        if branch:
            data["branch"] = branch
        if self.node_name:
            data["builder_node_id"] = self.node_name
        
        return self._request("PUT", f"/api/work-items/{ticket_id}", data)
    
    def complete_ticket(self, ticket_id: str, result: str, branch: str = "") -> dict:
        """Mark a ticket as done."""
        return self.update_ticket(ticket_id, "done", result=result, branch=branch)
    
    def fail_ticket(self, ticket_id: str, error: str) -> dict:
        """Mark a ticket as failed (back to todo)."""
        return self.update_ticket(ticket_id, "todo", error=error)
    
    def create_review_ticket(self, original_ticket_id: str, branch: str,
                             title: str, description: str = "") -> dict:
        """Create a review ticket for completed work."""
        return self._request("POST", "/api/work-items", {
            "title": f"[REVIEW] {title}",
            "description": description or f"Review code on branch: {branch}",
            "status": "ready_for_review",
            "priority": "high",
            "parent_id": original_ticket_id,
            "branch": branch,
            "builder_node_id": self.node_name,
        })
    
    def stats(self) -> dict:
        """Get ticket statistics."""
        tickets = self.get_tickets()
        
        by_status = {}
        for t in tickets:
            s = t.get("status", "unknown")
            by_status[s] = by_status.get(s, 0) + 1
        
        return {
            "total": len(tickets),
            "by_status": by_status,
            "claimable": len([t for t in tickets if t.get("status") in ("todo", "queued")]),
            "in_progress": len([t for t in tickets if t.get("status") == "in_progress"]),
            "blocked": len([t for t in tickets if t.get("status") == "blocked"]),
            "done": len([t for t in tickets if t.get("status") == "done"]),
            "review": len([t for t in tickets if t.get("status") == "ready_for_review"]),
        }
