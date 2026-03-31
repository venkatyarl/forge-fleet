"""Workload Manager — dynamically manage ticket concurrency.

Decides how many tickets to work on simultaneously based on:
- Number of available LLM endpoints
- Ticket complexity
- Current load on each endpoint
- Model sizes and tiers
"""
from dataclasses import dataclass, field
from concurrent.futures import ThreadPoolExecutor, as_completed
from .fleet_router import FleetRouter
from .mc_client import MCClient
from .pipeline import EngineeringPipeline, PipelineResult
from .task_decomposer import TaskDecomposer
from .ownership import OwnershipManager
from .execution_tracking import ExecutionTracker
from .lifecycle_policy import LifecyclePolicy
from .mcp_topology import MCPTopology


@dataclass
class WorkloadDecision:
    """Decision about how many tickets to run concurrently."""
    max_concurrent: int
    reasoning: str
    ticket_assignments: list = field(default_factory=list)


class WorkloadManager:
    """Dynamically manages ticket concurrency across the fleet.
    
    Simple tickets (create a struct) → run 10 at once
    Complex tickets (build auth system) → run 2 at once
    Adapts based on: endpoints available, LLM response times, model tiers
    """
    
    def __init__(self, repo_dir: str):
        self.repo_dir = repo_dir
        self.router = FleetRouter()
        self.mc = MCClient()
        self.decomposer = TaskDecomposer()
        self._init_tracker()
        self.active_tasks: dict = {}  # ticket_id -> future
        self.completed: list[PipelineResult] = []
        self.lifecycle = LifecyclePolicy()
        self.topology = MCPTopology.from_config()
        
        # Performance tracking
        self.avg_simple_time = 30  # seconds
        self.avg_complex_time = 300

    def _init_tracker(self):
        try:
            self.tracker = ExecutionTracker()
        except Exception:
            self.tracker = None
        self.ownership = OwnershipManager(
            node_name=self.mc.node_name,
            tracker=self.tracker,
        )
    
    def decide_concurrency(self, tickets: list[dict]) -> WorkloadDecision:
        """Decide how many tickets to run concurrently."""
        available = len(self.router.endpoints)
        
        if not tickets:
            return WorkloadDecision(0, "No tickets available")
        
        # Classify ticket complexity
        simple = []
        moderate = []
        complex_tickets = []
        
        for t in tickets:
            complexity = self._estimate_complexity(t)
            if complexity == "simple":
                simple.append(t)
            elif complexity == "moderate":
                moderate.append(t)
            else:
                complex_tickets.append(t)
        
        # Calculate optimal concurrency
        # Each complex ticket needs ~3 endpoints (reader + writer + reviewer)
        # Each simple ticket needs ~1 endpoint
        # Each moderate needs ~2
        
        complex_slots = len(complex_tickets) * 3
        moderate_slots = len(moderate) * 2
        simple_slots = len(simple)
        
        total_needed = complex_slots + moderate_slots + simple_slots
        
        if total_needed <= available:
            # Can run everything
            max_concurrent = len(tickets)
            reasoning = f"All {len(tickets)} tickets fit in {available} endpoints"
        elif complex_tickets:
            # Complex tickets dominate — limit concurrency
            max_concurrent = max(1, available // 3)
            reasoning = f"{len(complex_tickets)} complex tickets, limiting to {max_concurrent} concurrent (need 3 endpoints each)"
        else:
            # Moderate/simple mix
            max_concurrent = max(1, available // 2)
            reasoning = f"Mixed workload, running {max_concurrent} concurrent on {available} endpoints"
        
        return WorkloadDecision(
            max_concurrent=min(max_concurrent, len(tickets)),
            reasoning=reasoning,
        )
    
    def _estimate_complexity(self, ticket: dict) -> str:
        """Estimate ticket complexity from title/description."""
        title = ticket.get("title", "").lower()
        desc = ticket.get("description", "").lower()
        text = title + " " + desc
        
        # Complex indicators
        complex_keywords = ["system", "architecture", "integration", "workflow", "pipeline",
                          "multi-", "cross-", "migration", "refactor", "redesign"]
        if any(k in text for k in complex_keywords):
            return "complex"
        
        # Simple indicators
        simple_keywords = ["fix", "add field", "rename", "typo", "update", "doc comment",
                         "simple", "struct", "enum", "model"]
        if any(k in text for k in simple_keywords):
            return "simple"
        
        return "moderate"
    
    def run_batch(self, max_tickets: int = 20) -> list[PipelineResult]:
        """Run a batch of tickets with dynamic concurrency."""
        topology_validation = self.topology.validate(current_service="forgefleet")
        if not topology_validation.can_proceed:
            print(f"MCP topology blocked batch execution: {topology_validation.summary()}", flush=True)
            return []
        if topology_validation.degraded:
            print(f"⚠️ MCP topology degraded: {topology_validation.summary()}", flush=True)

        tickets = self.mc.get_claimable()
        if not tickets:
            print("No claimable tickets", flush=True)
            return []
        
        tickets = tickets[:max_tickets]
        decision = self.decide_concurrency(tickets)
        
        print(f"\n🎯 Workload Manager: {decision.reasoning}", flush=True)
        print(f"   Running {decision.max_concurrent} tickets concurrently", flush=True)
        
        results = []
        pipeline = EngineeringPipeline(self.repo_dir, ownership=self.ownership)
        
        with ThreadPoolExecutor(max_workers=decision.max_concurrent) as executor:
            futures = {}
            
            for ticket in tickets[:decision.max_concurrent]:
                ticket_id = ticket["id"]
                claimed, reason = self.ownership.claim(ticket_id)
                if not claimed:
                    print(f"  ⏭️ Skipping {ticket['title'][:50]} ({reason})", flush=True)
                    continue

                mc_claim = self.mc.claim_ticket(ticket_id)
                if isinstance(mc_claim, dict) and mc_claim.get("error"):
                    self.ownership.release(ticket_id, final_state="mc_claim_failed")
                    print(f"  ⏭️ Skipping {ticket['title'][:50]} (mc_claim_failed)", flush=True)
                    continue

                can_execute, exec_reason = self.ownership.can_execute(ticket_id)
                if not can_execute:
                    self.ownership.release(ticket_id, final_state=exec_reason)
                    print(f"  ⏭️ Skipping {ticket['title'][:50]} ({exec_reason})", flush=True)
                    continue

                future = executor.submit(pipeline.execute, ticket)
                futures[future] = ticket
                self.active_tasks[ticket_id] = future
            
            for future in as_completed(futures):
                ticket = futures[future]
                ticket_id = ticket["id"]
                try:
                    result = future.result()
                    results.append(result)
                    icon = "✅" if result.success else "❌"
                    final_state = result.done_state or (
                        result.final_state or (
                            "completed" if result.success else self.lifecycle.failure_state(execution_failed=True)
                        )
                    )
                    self.ownership.release(ticket_id, final_state=final_state)
                    self.active_tasks.pop(ticket_id, None)
                    print(f"  {icon} {result.title[:50]} ({result.total_time:.0f}s)", flush=True)
                except Exception as e:
                    self.ownership.release(
                        ticket_id,
                        final_state=self.lifecycle.failure_state(execution_failed=True),
                    )
                    self.active_tasks.pop(ticket_id, None)
                    print(f"  ❌ {ticket['title'][:50]}: {e}", flush=True)
        
        self.completed.extend(results)
        
        # Update performance tracking
        times = [r.total_time for r in results if r.success]
        if times:
            self.avg_simple_time = sum(times) / len(times)
        
        return results
    
    def status(self) -> dict:
        """Get workload manager status."""
        return {
            "active_tasks": len(self.active_tasks),
            "completed_total": len(self.completed),
            "success_rate": f"{sum(1 for r in self.completed if r.success)}/{len(self.completed)}" if self.completed else "N/A",
            "avg_task_time": f"{self.avg_simple_time:.0f}s",
            "fleet_endpoints": len(self.router.endpoints),
        }
