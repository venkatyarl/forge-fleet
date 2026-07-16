"""Revenue-Aware Scheduler — prioritize tasks that make money.

Item #12: If a freelance gig deadline is approaching, shift fleet resources.
Revenue-generating tasks get priority over internal projects.
"""
import time
from dataclasses import dataclass, field
from datetime import datetime
from .scheduler import WorkPriority, WorkItem


@dataclass
class RevenueTask:
    """A task with revenue implications."""
    description: str
    project: str
    revenue_potential: float  # Expected $ if completed
    deadline: float = 0  # Unix timestamp
    client: str = ""
    urgency: float = 0  # Calculated: higher = more urgent
    
    def calculate_urgency(self):
        """Calculate urgency based on deadline proximity + revenue."""
        if self.deadline:
            hours_left = (self.deadline - time.time()) / 3600
            if hours_left <= 0:
                self.urgency = 100  # OVERDUE
            elif hours_left < 24:
                self.urgency = 50 + self.revenue_potential / 100
            elif hours_left < 72:
                self.urgency = 25 + self.revenue_potential / 200
            else:
                self.urgency = self.revenue_potential / 500
        else:
            self.urgency = self.revenue_potential / 1000


class RevenueScheduler:
    """Prioritize revenue-generating work over internal projects.
    
    Priority order:
    1. Overdue client deliverables
    2. Client work due within 24h
    3. Revenue-generating features (paid subscriptions, marketplace)
    4. Internal product development
    5. Research and content
    """
    
    def __init__(self):
        self.revenue_tasks: list[RevenueTask] = []
    
    def add_revenue_task(self, task: RevenueTask):
        """Add a revenue-generating task."""
        task.calculate_urgency()
        self.revenue_tasks.append(task)
        self.revenue_tasks.sort(key=lambda t: -t.urgency)
    
    def get_priority_queue(self, internal_tasks: list[WorkItem] = None) -> list:
        """Merge revenue tasks with internal tasks, revenue gets priority."""
        queue = []
        
        # Revenue tasks first (by urgency)
        for rt in self.revenue_tasks:
            priority = WorkPriority.BUILD if rt.urgency > 25 else WorkPriority.REVENUE
            queue.append({
                "type": "revenue",
                "priority": priority.value,
                "description": rt.description,
                "project": rt.project,
                "revenue": rt.revenue_potential,
                "urgency": rt.urgency,
                "client": rt.client,
                "deadline": datetime.fromtimestamp(rt.deadline).isoformat() if rt.deadline else "none",
            })
        
        # Internal tasks after
        if internal_tasks:
            for it in internal_tasks:
                queue.append({
                    "type": "internal",
                    "priority": it.priority.value,
                    "description": it.description,
                    "project": it.project,
                })
        
        queue.sort(key=lambda t: t["priority"])
        return queue
    
    def summary(self) -> str:
        """Revenue task summary."""
        if not self.revenue_tasks:
            return "No revenue tasks in queue"
        
        total_revenue = sum(t.revenue_potential for t in self.revenue_tasks)
        overdue = sum(1 for t in self.revenue_tasks if t.urgency >= 100)
        urgent = sum(1 for t in self.revenue_tasks if 25 < t.urgency < 100)
        
        lines = [f"💰 Revenue Queue: {len(self.revenue_tasks)} tasks (${total_revenue:,.0f} potential)"]
        if overdue:
            lines.append(f"  🔴 {overdue} OVERDUE")
        if urgent:
            lines.append(f"  🟡 {urgent} urgent (due within 24-72h)")
        
        for t in self.revenue_tasks[:5]:
            deadline = datetime.fromtimestamp(t.deadline).strftime("%m/%d") if t.deadline else "no deadline"
            lines.append(f"  ${t.revenue_potential:,.0f} | {t.project} | {t.description[:40]} | {deadline}")
        
        return "\n".join(lines)
