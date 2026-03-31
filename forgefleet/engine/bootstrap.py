"""Bootstrap and enrollment helpers for new fleet nodes.

Phase 1 focuses on representing bootstrap targets and enrollment rules from
fleet.toml so ForgeFleet can reason about new nodes after the SSH/manual floor.
"""
from __future__ import annotations

from dataclasses import dataclass

from .. import config


@dataclass
class BootstrapTarget:
    name: str
    status: str
    reachable_by_ssh: bool
    enrolled: bool
    required_manual_floor: list[str]


class BootstrapManager:
    def __init__(self):
        self.enrollment = config.get_enrollment()

    def list_targets(self) -> list[BootstrapTarget]:
        targets = []
        for raw in config.get_bootstrap_targets():
            targets.append(BootstrapTarget(
                name=raw.get("name", "unknown"),
                status=raw.get("status", "pending"),
                reachable_by_ssh=bool(raw.get("reachable_by_ssh", False)),
                enrolled=bool(raw.get("enrolled", False)),
                required_manual_floor=list(raw.get("required_manual_floor", [])),
            ))
        return targets

    def can_bootstrap(self, target: BootstrapTarget) -> tuple[bool, str]:
        if self.enrollment.get("require_ssh_before_bootstrap", True) and not target.reachable_by_ssh:
            return False, "ssh_not_ready"
        if target.enrolled:
            return False, "already_enrolled"
        return True, "ok"
