"""Discover fleet nodes and models from fleet.json."""
import json
import subprocess
from pathlib import Path
from dataclasses import dataclass, field
from typing import Optional


@dataclass
class ModelEndpoint:
    name: str
    url: str
    port: int
    tier: int  # 1=fastest/smallest, higher=bigger/slower
    healthy: bool = False
    busy: bool = False


@dataclass  
class FleetNode:
    name: str
    ip: str
    ram_gb: int
    max_workers: int
    models: list[ModelEndpoint] = field(default_factory=list)
    ssh_user: str = ""
    connected: bool = False


class FleetDiscovery:
    """Discovers fleet topology from fleet.json."""
    
    def __init__(self, fleet_json_path: Optional[str] = None):
        self.nodes: dict[str, FleetNode] = {}
        self.tiers: dict[int, list[ModelEndpoint]] = {}
        
        # Find fleet.json
        paths = [
            fleet_json_path,
            str(Path.home() / "fleet.json"),
            str(Path.home() / ".openclaw/workspace/fleet.json"),
        ]
        for p in paths:
            if p and Path(p).exists():
                self._load(p)
                break
    
    def _load(self, path: str):
        """Load fleet topology from fleet.json."""
        with open(path) as f:
            fleet = json.load(f)
        
        # Load nodes
        for name, cfg in fleet.get("nodes", {}).items():
            node = FleetNode(
                name=name,
                ip=cfg.get("ip", ""),
                ram_gb=cfg.get("ram_gb", 0),
                max_workers=cfg.get("max_codex_agents", 2),
                ssh_user=cfg.get("ssh_user", ""),
            )
            
            # Parse model endpoints from llama_cpp config
            for model_desc in cfg.get("llama_cpp", {}).get("models", []):
                if "RPC worker" in model_desc:
                    continue
                # Extract port from description
                import re
                port_match = re.search(r'port (\d+)', model_desc)
                port = int(port_match.group(1)) if port_match else 51802
                
                # Determine tier based on model name
                tier = self._model_to_tier(model_desc)
                
                endpoint = ModelEndpoint(
                    name=model_desc.split("(")[0].strip(),
                    url=f"http://{cfg.get('ip', 'localhost')}:{port}",
                    port=port,
                    tier=tier,
                )
                node.models.append(endpoint)
                
                # Add to tier registry
                if tier not in self.tiers:
                    self.tiers[tier] = []
                self.tiers[tier].append(endpoint)
            
            self.nodes[name] = node
        
        # Load tiered pipeline config
        pipeline = fleet.get("inference", {}).get("tiered_pipeline", {})
        for tier_key, tier_cfg in pipeline.items():
            tier_num = int(tier_key.replace("tier", ""))
            # Add fleet_pool entries
            for url in tier_cfg.get("fleet_pool", []):
                endpoint = ModelEndpoint(
                    name=tier_cfg.get("model", "unknown"),
                    url=url,
                    port=int(url.split(":")[-1]) if ":" in url else 51802,
                    tier=tier_num,
                )
                if tier_num not in self.tiers:
                    self.tiers[tier_num] = []
                self.tiers[tier_num].append(endpoint)
    
    def _model_to_tier(self, desc: str) -> int:
        """Map model description to tier number."""
        desc_lower = desc.lower()
        if "9b" in desc_lower:
            return 1
        elif "32b" in desc_lower or "coder" in desc_lower:
            return 2
        elif "72b" in desc_lower:
            return 3
        elif "235b" in desc_lower or "cluster" in desc_lower:
            return 4
        return 2  # default to tier 2
    
    def health_check(self, endpoint: ModelEndpoint) -> bool:
        """Check if a model endpoint is healthy."""
        try:
            r = subprocess.run(
                ["curl", "-s", "--max-time", "3", f"{endpoint.url}/health"],
                capture_output=True, text=True, timeout=5
            )
            if r.returncode == 0 and '"ok"' in r.stdout:
                endpoint.healthy = True
                return True
        except:
            pass
        endpoint.healthy = False
        return False
    
    def check_busy(self, endpoint: ModelEndpoint) -> bool:
        """Check if model is currently processing a request."""
        try:
            r = subprocess.run(
                ["curl", "-s", "--max-time", "2", f"{endpoint.url}/slots"],
                capture_output=True, text=True, timeout=4
            )
            if r.returncode == 0 and r.stdout.strip():
                import json as _json
                slots = _json.loads(r.stdout)
                # llama.cpp uses "is_processing": true/false
                if isinstance(slots, list):
                    all_busy = all(s.get("is_processing", False) for s in slots)
                    endpoint.busy = all_busy
                    return all_busy
        except:
            pass
        endpoint.busy = False
        return False
    
    def get_available(self, tier: int, prefer_local: str = "") -> list[ModelEndpoint]:
        """Get available endpoints for a tier, sorted by preference."""
        candidates = self.tiers.get(tier, [])
        available = []
        
        for ep in candidates:
            if self.health_check(ep) and not self.check_busy(ep):
                available.append(ep)
        
        # Sort: local first, then by name
        if prefer_local:
            available.sort(key=lambda e: 0 if prefer_local in e.url else 1)
        
        return available
    
    def discover_all(self) -> dict:
        """Run health checks on entire fleet, return status."""
        status = {}
        for name, node in self.nodes.items():
            node_status = {"models": [], "healthy": 0, "total": 0}
            for ep in node.models:
                healthy = self.health_check(ep)
                node_status["models"].append({
                    "name": ep.name,
                    "url": ep.url,
                    "tier": ep.tier,
                    "healthy": healthy,
                })
                node_status["total"] += 1
                if healthy:
                    node_status["healthy"] += 1
            node_status["connected"] = node_status["healthy"] > 0
            status[name] = node_status
        return status
