"""Scheduling policy helpers for capability/load/ownership-aware routing."""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Optional

from .. import config


@dataclass
class TaskRequirements:
    task_type: str = "general"
    required_capabilities: list[str] = field(default_factory=list)
    preferred_workloads: list[str] = field(default_factory=list)
    min_ram_gb: int = 0
    min_vram_gb: int = 0
    requires_gpu: bool = False
    requires_docker: bool = False
    estimated_heavy: bool = False


@dataclass
class TaskLease:
    task_id: str
    owner: str
    handoff_count: int = 0
    claimed_ram_gb: int = 0
    claimed_gpu: bool = False


class SchedulingPolicy:
    """Simple first-pass capability and preference policy.

    Phase 1 goals:
    - filter ineligible nodes
    - apply soft preference scoring
    - provide ownership/handoff metadata shape
    """

    def node_eligible(self, node_name: str, req: TaskRequirements) -> tuple[bool, str]:
        caps = config.get_node_capabilities(node_name)
        node = config.get_node(node_name)
        resources = node.get("resources", {}) if isinstance(node, dict) else {}

        if req.requires_gpu and not caps.get("gpu", False) and not caps.get("premium_inference", False) and not caps.get("model_building", False):
            return False, "gpu_required"
        if req.requires_docker and not caps.get("docker", False):
            return False, "docker_required"

        ram_gb = int(resources.get("ram_gb", 0) or 0)
        vram_gb = int(resources.get("vram_gb", 0) or 0)
        if req.min_ram_gb and ram_gb and ram_gb < req.min_ram_gb:
            return False, "insufficient_ram"
        if req.min_vram_gb and vram_gb < req.min_vram_gb:
            return False, "insufficient_vram"

        for needed in req.required_capabilities:
            if not caps.get(needed, False):
                return False, f"missing_capability:{needed}"

        return True, "ok"

    def preference_score(self, node_name: str, req: TaskRequirements) -> float:
        prefs = config.get_node_preferences(node_name)
        preferred = prefs.get("preferred_workloads", []) if isinstance(prefs, dict) else []
        first_pref = prefs.get("first_preference_workloads", []) if isinstance(prefs, dict) else []

        score = 0.0
        if req.task_type in first_pref:
            score += 5.0
        if req.task_type in preferred:
            score += 2.0
        for workload in req.preferred_workloads:
            if workload in first_pref:
                score += 3.0
            elif workload in preferred:
                score += 1.0
        return score

    def score_node(self, node_name: str, req: TaskRequirements, current_load: Optional[dict[str, Any]] = None) -> float:
        current_load = current_load or {}
        score = self.preference_score(node_name, req)

        # Lighter load is better
        cpu = float(current_load.get("cpu_load", 0.0) or 0.0)
        ram = float(current_load.get("ram_pressure", 0.0) or 0.0)
        busy = 1.0 if current_load.get("busy", False) else 0.0

        score -= cpu * 2.0
        score -= ram * 2.0
        score -= busy * 3.0

        # Favor stronger/faster nodes when they are actually available
        node = config.get_node(node_name)
        resources = node.get("resources", {}) if isinstance(node, dict) else {}
        cpu_cores = int(resources.get("cpu_cores", 0) or 0)
        ram_gb = int(resources.get("ram_gb", 0) or 0)
        vram_gb = int(resources.get("vram_gb", 0) or 0)
        score += min(cpu_cores / 16.0, 2.0)
        score += min(ram_gb / 64.0, 2.0)
        score += min(vram_gb / 24.0, 2.0)

        return score
