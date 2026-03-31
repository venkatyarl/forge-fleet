"""Software, update policy, and maintenance state foundation for ForgeFleet.

This module provides the core data shapes for tracking what is installed on each
node, how versions drift over time, when updates should run, and whether a node
is currently in a maintenance window.
"""
from __future__ import annotations

import time
from dataclasses import asdict, dataclass, field
from typing import Any


@dataclass
class VersionTrack:
    """Desired/current version tracking for a single software unit."""
    current: str = ""
    desired: str = ""
    channel: str = "stable"
    source: str = ""
    detected_at: float = field(default_factory=time.time)
    updated_at: float = field(default_factory=time.time)

    @property
    def drifted(self) -> bool:
        return bool(self.current and self.desired and self.current != self.desired)

    def touch(self):
        self.updated_at = time.time()


@dataclass
class InstalledSoftware:
    """Represents one installed package, service, runtime, or model."""
    name: str
    category: str = "package"  # package|service|runtime|model|binary|os_component
    manager: str = "manual"    # brew|apt|dnf|pip|npm|docker|manual|system
    version: str = ""
    desired_version: str = ""
    state: str = "installed"   # installed|available|removed|failed|pending
    install_path: str = ""
    installed_at: float = field(default_factory=time.time)
    last_checked_at: float = field(default_factory=time.time)
    metadata: dict[str, Any] = field(default_factory=dict)

    def refresh(self, version: str = "", state: str = ""):
        if version:
            self.version = version
        if state:
            self.state = state
        self.last_checked_at = time.time()


@dataclass
class OperatingSystemState:
    """OS identity and patch level for a node."""
    family: str = ""
    name: str = ""
    version: str = ""
    kernel: str = ""
    architecture: str = ""
    patch_level: str = ""
    last_reboot_at: float = 0.0
    last_patch_at: float = 0.0


@dataclass
class UpdatePolicy:
    """Describes when and how a node should accept updates."""
    strategy: str = "manual"           # manual|scheduled|rolling|automatic
    cadence: str = "weekly"            # immediate|daily|weekly|monthly|manual
    maintenance_window: str = ""
    auto_download: bool = False
    auto_apply: bool = False
    allow_major: bool = False
    require_approval: bool = True
    reboot_strategy: str = "manual"    # manual|when-required|always|never
    rollout_group: str = "default"
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class MaintenanceState:
    """Tracks whether a node is safe to update or currently being serviced."""
    mode: str = "active"               # active|draining|maintenance|disabled
    reason: str = ""
    initiated_by: str = ""
    ticket_id: str = ""
    started_at: float = 0.0
    ends_at: float = 0.0
    drain_workloads: bool = False
    reboot_required: bool = False
    pending_updates: list[str] = field(default_factory=list)
    blocked_actions: list[str] = field(default_factory=list)

    @property
    def in_maintenance(self) -> bool:
        return self.mode in {"draining", "maintenance", "disabled"}


@dataclass
class NodeSoftwareInventory:
    """Full software/update state snapshot for one ForgeFleet node."""
    node_name: str
    os: OperatingSystemState = field(default_factory=OperatingSystemState)
    software: dict[str, InstalledSoftware] = field(default_factory=dict)
    versions: dict[str, VersionTrack] = field(default_factory=dict)
    update_policy: UpdatePolicy = field(default_factory=UpdatePolicy)
    maintenance: MaintenanceState = field(default_factory=MaintenanceState)
    discovered_at: float = field(default_factory=time.time)
    updated_at: float = field(default_factory=time.time)
    metadata: dict[str, Any] = field(default_factory=dict)

    def upsert_software(self, item: InstalledSoftware):
        self.software[item.name] = item
        self.updated_at = time.time()
        self._sync_version_track(item)

    def record_version(self, name: str, current: str = "", desired: str = "",
                       channel: str = "stable", source: str = "") -> VersionTrack:
        track = self.versions.get(name)
        if not track:
            track = VersionTrack(
                current=current,
                desired=desired,
                channel=channel,
                source=source,
            )
            self.versions[name] = track
        else:
            if current:
                track.current = current
            if desired:
                track.desired = desired
            if channel:
                track.channel = channel
            if source:
                track.source = source
            track.touch()
        self.updated_at = time.time()
        return track

    def set_maintenance(self, mode: str, reason: str = "", initiated_by: str = "",
                        ticket_id: str = "", ends_at: float = 0.0,
                        drain_workloads: bool = False):
        self.maintenance.mode = mode
        self.maintenance.reason = reason
        self.maintenance.initiated_by = initiated_by
        self.maintenance.ticket_id = ticket_id
        self.maintenance.started_at = time.time()
        self.maintenance.ends_at = ends_at
        self.maintenance.drain_workloads = drain_workloads
        self.updated_at = time.time()

    def clear_maintenance(self):
        self.maintenance = MaintenanceState()
        self.updated_at = time.time()

    def mark_pending_update(self, name: str, reboot_required: bool = False):
        if name not in self.maintenance.pending_updates:
            self.maintenance.pending_updates.append(name)
        self.maintenance.reboot_required = self.maintenance.reboot_required or reboot_required
        self.updated_at = time.time()

    def pending_update_names(self) -> list[str]:
        names = set(self.maintenance.pending_updates)
        for name, track in self.versions.items():
            if track.drifted:
                names.add(name)
        return sorted(names)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    def _sync_version_track(self, item: InstalledSoftware):
        desired = item.desired_version or self.versions.get(item.name, VersionTrack()).desired
        self.record_version(
            name=item.name,
            current=item.version,
            desired=desired,
            channel=self.versions.get(item.name, VersionTrack()).channel,
            source=item.manager,
        )
