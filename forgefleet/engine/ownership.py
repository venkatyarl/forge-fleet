"""Ownership, lease, and handoff management for distributed task execution.

Core model:
- One owner per ticket (single-threaded accountability)
- Many contributors/reviewers allowed (multi-threaded execution)
- Explicit handoff (changes owner)
- Explicit escalation (moves upward: intern → junior → senior → executive → human)
- All state persists to ForgeFleet Postgres via ExecutionTracker
"""
from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Optional

from .. import config


ESCALATION_LADDER = ["intern", "junior", "senior", "executive", "human"]


@dataclass
class TaskOwnership:
    """Ownership and collaboration state for a single ticket."""

    ticket_id: str
    owner: str
    owner_level: str = "junior"
    claimed_at: float = 0.0
    lease_seconds: int = 1800
    handoff_count: int = 0
    escalation_count: int = 0
    source_owner: str = ""
    state: str = "claimed"
    status_reason: str = ""
    contributors: list[str] = field(default_factory=list)
    reviewers: list[str] = field(default_factory=list)
    escalation_path: list[str] = field(default_factory=list)
    last_model: dict = field(default_factory=dict)

    @property
    def expires_at(self) -> float:
        return self.claimed_at + self.lease_seconds

    def is_expired(self) -> bool:
        return time.time() > self.expires_at


class OwnershipManager:
    """Manages task ownership with collaboration, handoff, and escalation.

    Integrates with ExecutionTracker for Postgres persistence.
    """

    def __init__(self, node_name: str = "", max_handoffs: int = 3,
                 lease_seconds: int = 1800, tracker=None):
        self.node_name = node_name or config.get_node_name()
        self.max_handoffs = max_handoffs
        self.lease_seconds = lease_seconds
        self.tracker = tracker
        self.tasks: dict[str, TaskOwnership] = {}

    def claim(self, ticket_id: str, owner_level: str = "junior") -> tuple[bool, str]:
        existing = self.tasks.get(ticket_id)
        if existing and not existing.is_expired() and existing.owner != self.node_name:
            return False, f"owned_by:{existing.owner}"

        task = TaskOwnership(
            ticket_id=ticket_id,
            owner=self.node_name,
            owner_level=owner_level,
            claimed_at=time.time(),
            lease_seconds=self.lease_seconds,
            handoff_count=existing.handoff_count if existing else 0,
            escalation_count=existing.escalation_count if existing else 0,
            source_owner=existing.owner if existing else "",
            state="claimed",
        )
        self.tasks[ticket_id] = task
        self._persist(task, "claimed")
        return True, "claimed"

    def add_contributor(self, ticket_id: str, contributor: str) -> tuple[bool, str]:
        task = self.tasks.get(ticket_id)
        if not task:
            return False, "no_task"
        if contributor not in task.contributors:
            task.contributors.append(contributor)
        self._persist(task, "contributor_added", details={"contributor": contributor})
        return True, "contributor_added"

    def add_reviewer(self, ticket_id: str, reviewer: str) -> tuple[bool, str]:
        task = self.tasks.get(ticket_id)
        if not task:
            return False, "no_task"
        if reviewer not in task.reviewers:
            task.reviewers.append(reviewer)
        self._persist(task, "reviewer_added", details={"reviewer": reviewer})
        return True, "reviewer_added"

    def handoff(self, ticket_id: str, new_owner: str,
                new_level: str = "") -> tuple[bool, str]:
        task = self.tasks.get(ticket_id)
        if not task:
            return False, "no_task"
        if task.handoff_count >= self.max_handoffs:
            return False, "handoff_limit_reached"

        task.source_owner = task.owner
        task.owner = new_owner
        if new_level:
            task.owner_level = new_level
        task.handoff_count += 1
        task.claimed_at = time.time()
        task.state = "handed_off"
        self._persist(task, "handed_off", details={
            "from": task.source_owner, "to": new_owner, "level": task.owner_level,
        })
        return True, "handed_off"

    def escalate(self, ticket_id: str, new_owner: str = "") -> tuple[bool, str]:
        task = self.tasks.get(ticket_id)
        if not task:
            return False, "no_task"

        current_idx = ESCALATION_LADDER.index(task.owner_level) \
            if task.owner_level in ESCALATION_LADDER else 0
        if current_idx >= len(ESCALATION_LADDER) - 1:
            return False, "already_at_top"

        next_level = ESCALATION_LADDER[current_idx + 1]
        task.escalation_path.append(f"{task.owner}@{task.owner_level}")
        task.escalation_count += 1

        old_owner = task.owner
        if new_owner:
            task.owner = new_owner
        task.owner_level = next_level
        task.state = "escalated"
        task.claimed_at = time.time()
        self._persist(task, "escalated", details={
            "from_level": ESCALATION_LADDER[current_idx],
            "to_level": next_level,
            "from_owner": old_owner,
            "to_owner": task.owner,
        })
        return True, f"escalated_to_{next_level}"

    def record_model(self, ticket_id: str, stage: str, model_name: str,
                     node_name: str, role: str,
                     details: dict | None = None) -> tuple[bool, str]:
        task = self.tasks.get(ticket_id)
        if not task:
            return False, "no_task"
        task.last_model = {"model": model_name, "node": node_name, "stage": stage}
        if self.tracker:
            self.tracker.log_model_usage(
                ticket_id=ticket_id, stage=stage, model_name=model_name,
                node_name=node_name, role=role, details=details,
            )
        return True, "model_recorded"

    def renew(self, ticket_id: str) -> tuple[bool, str]:
        task = self.tasks.get(ticket_id)
        if not task:
            return False, "no_task"
        task.claimed_at = time.time()
        task.state = "renewed"
        self._persist(task, "renewed")
        return True, "renewed"

    def release(self, ticket_id: str,
                final_state: str = "released") -> tuple[bool, str]:
        task = self.tasks.get(ticket_id)
        if not task:
            return False, "no_task"
        task.state = final_state
        self._persist(task, final_state)
        del self.tasks[ticket_id]
        return True, final_state

    def can_execute(self, ticket_id: str) -> tuple[bool, str]:
        task = self.tasks.get(ticket_id)
        if not task:
            return False, "no_task"
        if task.owner != self.node_name:
            return False, f"owned_by:{task.owner}"
        if task.is_expired():
            return False, "lease_expired"
        return True, "ok"

    def get_task(self, ticket_id: str) -> Optional[TaskOwnership]:
        return self.tasks.get(ticket_id)

    def _persist(self, task: TaskOwnership, event_type: str,
                 details: dict | None = None):
        if not self.tracker:
            return
        self.tracker.upsert_execution(
            ticket_id=task.ticket_id,
            current_owner=task.owner,
            owner_level=task.owner_level,
            state=task.state,
            status_reason=task.status_reason,
            handoff_count=task.handoff_count,
            escalation_count=task.escalation_count,
            contributors=task.contributors,
            reviewers=task.reviewers,
            escalation_path=task.escalation_path,
            last_model=task.last_model,
        )
        self.tracker.log_event(
            ticket_id=task.ticket_id,
            event_type=event_type,
            actor=task.owner,
            details=details,
        )
