"""MCP topology loading and runtime validation for ForgeFleet.

The canonical source is `[mcp]` in `~/.forgefleet/fleet.toml` via
`forgefleet.config.get_mcp_topology()`.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Iterable

from .. import config


@dataclass
class MCPService:
    name: str
    server: bool = False
    client: bool = False
    port: int = 0
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class MCPLink:
    source: str
    target: str
    required: bool = True
    reason: str = ""

    @property
    def label(self) -> str:
        return f"{self.source}->{self.target}"


@dataclass
class TopologyValidation:
    services: dict[str, dict[str, Any]] = field(default_factory=dict)
    available_required: list[str] = field(default_factory=list)
    available_optional: list[str] = field(default_factory=list)
    missing_required: list[str] = field(default_factory=list)
    missing_optional: list[str] = field(default_factory=list)

    @property
    def can_proceed(self) -> bool:
        return not self.missing_required

    @property
    def degraded(self) -> bool:
        return bool(self.missing_optional)

    def summary(self) -> str:
        if self.missing_required:
            return f"Missing required MCP links: {', '.join(self.missing_required)}"
        if self.missing_optional:
            return f"Missing optional MCP links: {', '.join(self.missing_optional)}"
        return "MCP topology satisfied"

    def to_dict(self) -> dict[str, Any]:
        return {
            "services": self.services,
            "available_required": self.available_required,
            "available_optional": self.available_optional,
            "missing_required": self.missing_required,
            "missing_optional": self.missing_optional,
            "can_proceed": self.can_proceed,
            "degraded": self.degraded,
            "summary": self.summary(),
        }


@dataclass
class MCPTopology:
    services: dict[str, MCPService] = field(default_factory=dict)
    required_links: list[MCPLink] = field(default_factory=list)
    optional_links: list[MCPLink] = field(default_factory=list)

    @classmethod
    def from_config(cls, raw: dict[str, Any] | None = None) -> "MCPTopology":
        raw = raw if raw is not None else config.get_mcp_topology()
        services: dict[str, MCPService] = {}

        for name, service_cfg in raw.items():
            if name in {"links", "required_links", "optional_links"}:
                continue
            if not isinstance(service_cfg, dict):
                continue
            services[name] = MCPService(
                name=name,
                server=bool(service_cfg.get("server", False)),
                client=bool(service_cfg.get("client", False)),
                port=int(service_cfg.get("port", 0) or 0),
                metadata={
                    key: value
                    for key, value in service_cfg.items()
                    if key not in {"server", "client", "port"}
                },
            )

        links_cfg = raw.get("links", {}) if isinstance(raw.get("links", {}), dict) else {}
        required_links = cls._parse_links(
            links_cfg.get("required") or raw.get("required_links") or [] ,
            required=True,
        )
        optional_links = cls._parse_links(
            links_cfg.get("optional") or raw.get("optional_links") or [],
            required=False,
        )

        if not required_links and "forgefleet" in services and "mc" in services:
            required_links.append(MCPLink("forgefleet", "mc", required=True, reason="mission_control"))
        if not optional_links and "forgefleet" in services and "openclaw" in services:
            optional_links.append(MCPLink("forgefleet", "openclaw", required=False, reason="operator_assist"))

        return cls(services=services, required_links=required_links, optional_links=optional_links)

    @staticmethod
    def _parse_links(values: Iterable[Any], required: bool) -> list[MCPLink]:
        links: list[MCPLink] = []
        for value in values or []:
            if isinstance(value, MCPLink):
                links.append(MCPLink(value.source, value.target, required=required, reason=value.reason))
                continue
            if isinstance(value, str):
                source, target = MCPTopology._split_link(value)
                if source and target:
                    links.append(MCPLink(source, target, required=required))
                continue
            if isinstance(value, (tuple, list)) and len(value) >= 2:
                links.append(MCPLink(str(value[0]), str(value[1]), required=required))
                continue
            if isinstance(value, dict):
                source = str(value.get("from") or value.get("source") or "").strip()
                target = str(value.get("to") or value.get("target") or "").strip()
                if source and target:
                    links.append(MCPLink(
                        source=source,
                        target=target,
                        required=required,
                        reason=str(value.get("reason", "")),
                    ))
        return links

    @staticmethod
    def _split_link(value: str) -> tuple[str, str]:
        normalized = value.strip().replace(":", "->")
        if "->" not in normalized:
            return "", ""
        source, target = normalized.split("->", 1)
        return source.strip(), target.strip()

    def service_state(self) -> dict[str, dict[str, Any]]:
        return {
            name: {
                "server": service.server,
                "client": service.client,
                "port": service.port,
            }
            for name, service in self.services.items()
        }

    def link_available(self, source: str, target: str) -> bool:
        source_service = self.services.get(source)
        target_service = self.services.get(target)
        if not source_service or not target_service:
            return False
        return bool(source_service.client and target_service.server)

    def validate(self, current_service: str = "", required_links: Iterable[Any] | None = None,
                 optional_links: Iterable[Any] | None = None) -> TopologyValidation:
        required = self._parse_links(required_links, required=True) if required_links is not None else list(self.required_links)
        optional = self._parse_links(optional_links, required=False) if optional_links is not None else list(self.optional_links)

        if current_service:
            required = [link for link in required if not link.source or link.source == current_service]
            optional = [link for link in optional if not link.source or link.source == current_service]

        validation = TopologyValidation(services=self.service_state())

        for link in required:
            if self.link_available(link.source, link.target):
                validation.available_required.append(link.label)
            else:
                validation.missing_required.append(link.label)

        for link in optional:
            if self.link_available(link.source, link.target):
                validation.available_optional.append(link.label)
            else:
                validation.missing_optional.append(link.label)

        return validation
