"""Autonomous task lifecycle policy for ForgeFleet.

Phase 1 policy defaults:
- gated auto-merge eligibility
- definition of done
- retry/review loop limits
- failure state helpers
"""
from __future__ import annotations

from dataclasses import dataclass, field


DEFAULT_HUMAN_REVIEW_CLASSES = {
    "security",
    "auth",
    "billing",
    "payments",
    "db_migration",
    "secrets",
    "fleet_governance",
}


@dataclass
class MergeContext:
    task_type: str = "general"
    tests_passed: bool = False
    review_passed: bool = False
    has_blocking_feedback: bool = False
    branch_mergeable: bool = False
    human_review_required: bool = False
    blocked_by_policy: bool = False


@dataclass
class LifecyclePolicy:
    max_execution_retries: int = 2
    max_review_loops: int = 2
    human_review_classes: set[str] = field(default_factory=lambda: set(DEFAULT_HUMAN_REVIEW_CLASSES))

    def can_auto_merge(self, ctx: MergeContext) -> tuple[bool, str]:
        if ctx.task_type in self.human_review_classes:
            return False, "human_review_class"
        if ctx.human_review_required:
            return False, "human_review_required"
        if ctx.blocked_by_policy:
            return False, "blocked_by_policy"
        if not ctx.tests_passed:
            return False, "tests_not_passed"
        if not ctx.review_passed:
            return False, "review_not_passed"
        if ctx.has_blocking_feedback:
            return False, "blocking_feedback"
        if not ctx.branch_mergeable:
            return False, "branch_not_mergeable"
        return True, "ok"

    def done_state(self, merged: bool, mc_updated: bool, review_passed: bool, tests_passed: bool) -> str:
        if merged and mc_updated and review_passed and tests_passed:
            return "done"
        if tests_passed and review_passed:
            return "ready_to_merge"
        if tests_passed:
            return "tested"
        return "in_progress"

    def should_retry_execution(self, retries: int) -> bool:
        return retries < self.max_execution_retries

    def should_retry_review(self, loops: int) -> bool:
        return loops < self.max_review_loops

    def failure_state(self, failed_test: bool = False, failed_review: bool = False,
                      blocked: bool = False, needs_human: bool = False) -> str:
        if needs_human:
            return "needs_human"
        if blocked:
            return "blocked"
        if failed_review:
            return "failed_review"
        if failed_test:
            return "failed_test"
        return "retrying"
