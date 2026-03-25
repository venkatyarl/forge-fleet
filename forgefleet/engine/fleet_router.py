"""Fleet Router — concurrent multi-node execution with automatic escalation.

Handles:
1. Parallel execution across fleet nodes
2. Busy detection via /slots API
3. Automatic escalation: 9B → 32B → 72B → 235B
4. Tier handoff: lower model scaffolds, higher model finishes
5. Load balancing across same-tier models
"""
import json
import os
import time
import urllib.request
import urllib.error
from dataclasses import dataclass, field
from concurrent.futures import ThreadPoolExecutor, as_completed
from typing import Optional
from .llm import LLM


@dataclass
class ModelEndpoint:
    """A model running on a specific node."""
    name: str
    node: str
    ip: str
    port: int
    tier: int  # 1=9B, 2=32B, 3=72B, 4=235B
    url: str = ""
    busy: bool = False
    healthy: bool = True
    last_check: float = 0
    
    def __post_init__(self):
        if not self.url:
            self.url = f"http://{self.ip}:{self.port}"


@dataclass
class FleetRouter:
    """Routes tasks to the best available model across the fleet.
    
    Features:
    - Discovers all models from fleet.json
    - Checks health + busy status before routing
    - Picks least-loaded endpoint for each tier
    - Escalates to next tier if current tier fails or is all busy
    - Supports concurrent execution across multiple nodes
    """
    fleet_path: str = ""
    endpoints: list = field(default_factory=list)
    tiers: dict = field(default_factory=dict)  # tier_num -> [endpoints]
    _loaded: bool = False
    
    def __post_init__(self):
        if not self.fleet_path:
            for path in [
                os.path.expanduser("~/fleet.json"),
                os.path.expanduser("~/.openclaw/workspace/fleet.json"),
                "/Users/venkat/.openclaw/workspace/fleet.json",
            ]:
                if os.path.exists(path):
                    self.fleet_path = path
                    break
        self._load_fleet()
        
        # If no models found from fleet.json (v2 simplified), use discovery
        if not self.endpoints:
            self._discover_models()
    
    def _load_fleet(self):
        """Load fleet.json and build endpoint registry.
        
        Handles two formats:
        1. Structured: nodes.X.models = [{name, port, tier}]
        2. String: nodes.X.llama_cpp.models = ["Qwen3.5-9B (port 8082) — active"]
        """
        import re
        
        if not self.fleet_path or not os.path.exists(self.fleet_path):
            return
        
        with open(self.fleet_path) as f:
            fleet = json.load(f)
        
        for node_name, node in fleet.get("nodes", {}).items():
            ip = node.get("ip", "127.0.0.1")
            
            # Try structured format first
            structured_models = node.get("models", [])
            if structured_models and isinstance(structured_models[0], dict):
                for model in structured_models:
                    ep = ModelEndpoint(
                        name=model.get("name", "unknown"),
                        node=node_name, ip=ip,
                        port=model.get("port", 51800),
                        tier=model.get("tier", 1),
                    )
                    self.endpoints.append(ep)
                    self.tiers.setdefault(ep.tier, []).append(ep)
                continue
            
            # Parse string format from llama_cpp.models
            llama_models = node.get("llama_cpp", {}).get("models", [])
            for m in llama_models:
                if not isinstance(m, str):
                    continue
                
                # Skip RPC workers and inactive models
                if "rpc" in m.lower() or "active" not in m.lower():
                    continue
                
                # Extract port: "port 8082"
                port_match = re.search(r"port (\d+)", m)
                port = int(port_match.group(1)) if port_match else 51800
                
                # Extract model name: everything before first "("
                model_name = m.split("(")[0].strip()
                
                # Determine tier from model size
                tier = 1
                if "235B" in m or "397B" in m:
                    tier = 4
                elif "72B" in m:
                    tier = 3
                elif "32B" in m:
                    tier = 2
                elif "9B" in m or "14B" in m:
                    tier = 1
                
                ep = ModelEndpoint(
                    name=model_name, node=node_name, ip=ip,
                    port=port, tier=tier,
                )
                self.endpoints.append(ep)
                self.tiers.setdefault(ep.tier, []).append(ep)
        
        self._loaded = True
    
    def _discover_models(self):
        """Fall back to network discovery when fleet.json has no model listings."""
        from .discovery import NetworkDiscovery
        disc = NetworkDiscovery()
        discovered = disc.scan_known_hosts()
        
        for ep in discovered:
            model_ep = ModelEndpoint(
                name=ep.model_name,
                node=ep.hostname or ep.ip,
                ip=ep.ip,
                port=ep.port,
                tier=ep.tier,
            )
            self.endpoints.append(model_ep)
            self.tiers.setdefault(model_ep.tier, []).append(model_ep)
        
        if discovered:
            self._loaded = True
    
    def check_health(self, ep: ModelEndpoint) -> bool:
        """Check if endpoint is reachable."""
        try:
            req = urllib.request.Request(f"{ep.url}/health")
            with urllib.request.urlopen(req, timeout=3) as resp:
                ep.healthy = resp.status == 200
        except Exception:
            ep.healthy = False
        ep.last_check = time.time()
        return ep.healthy
    
    def check_busy(self, ep: ModelEndpoint) -> bool:
        """Check if endpoint is currently processing a request."""
        try:
            req = urllib.request.Request(f"{ep.url}/slots")
            with urllib.request.urlopen(req, timeout=3) as resp:
                slots = json.loads(resp.read())
                if isinstance(slots, list):
                    ep.busy = any(s.get("is_processing", False) for s in slots)
                else:
                    ep.busy = False
        except Exception:
            ep.busy = False  # Assume not busy if we can't check
        return ep.busy
    
    def get_available(self, tier: int) -> list[ModelEndpoint]:
        """Get available (healthy + not busy) endpoints for a tier."""
        candidates = self.tiers.get(tier, [])
        available = []
        
        for ep in candidates:
            # Re-check if stale (>30s since last check)
            if time.time() - ep.last_check > 30:
                self.check_health(ep)
                if ep.healthy:
                    self.check_busy(ep)
            
            if ep.healthy and not ep.busy:
                available.append(ep)
        
        return available
    
    def get_llm(self, tier: int, fallback_up: bool = True) -> Optional[LLM]:
        """Get an LLM for the requested tier, escalating if needed.
        
        Args:
            tier: Requested tier (1=9B, 2=32B, 3=72B, 4=235B)
            fallback_up: If True, try higher tiers when requested tier is busy
            
        Returns:
            LLM instance or None if nothing available
        """
        # Try requested tier first
        available = self.get_available(tier)
        if available:
            ep = available[0]  # Pick first available (could add random/round-robin)
            return LLM(
                base_url=f"{ep.url}/v1",
                model=ep.name,
                timeout=900 if tier >= 3 else 300,
            )
        
        # Escalate to higher tiers if allowed
        if fallback_up:
            for higher_tier in range(tier + 1, 5):
                available = self.get_available(higher_tier)
                if available:
                    ep = available[0]
                    return LLM(
                        base_url=f"{ep.url}/v1",
                        model=ep.name,
                        timeout=900,
                    )
        
        return None
    
    def get_llm_for_role(self, role: str) -> LLM:
        """Map agent roles to appropriate model tiers.
        
        Role mapping:
        - "context", "research", "scaffold" → Tier 1 (9B, fast)
        - "code", "write", "implement" → Tier 2 (32B, quality)
        - "review", "verify", "test" → Tier 3 (72B, smart)
        - "architect", "complex", "expert" → Tier 4 (235B, expert)
        """
        role_lower = role.lower()
        
        if any(k in role_lower for k in ["context", "research", "scaffold", "fast", "scan"]):
            tier = 1
        elif any(k in role_lower for k in ["code", "write", "implement", "develop", "build"]):
            tier = 2
        elif any(k in role_lower for k in ["review", "verify", "test", "check", "qa"]):
            tier = 3
        elif any(k in role_lower for k in ["architect", "complex", "expert", "plan", "design"]):
            tier = 4
        else:
            tier = 1  # Default to fast model
        
        llm = self.get_llm(tier)
        if llm is None:
            # Nothing available at any tier — use a hardcoded fallback
            llm = LLM(base_url="http://192.168.5.100:51803/v1", model="fallback")
        
        return llm
    
    def execute_parallel(self, tasks: list[dict], max_workers: int = 4) -> list[dict]:
        """Execute multiple tasks concurrently across fleet nodes.
        
        Each task dict has:
        - "description": task text
        - "tier": preferred model tier
        - "func": callable that takes an LLM and returns result
        
        Returns list of result dicts.
        """
        results = []
        
        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            futures = {}
            
            for i, task in enumerate(tasks):
                tier = task.get("tier", 1)
                llm = self.get_llm(tier)
                
                if llm is None:
                    results.append({
                        "task": i, "success": False,
                        "error": f"No available model for tier {tier}",
                    })
                    continue
                
                func = task["func"]
                future = executor.submit(func, llm)
                futures[future] = i
            
            for future in as_completed(futures):
                idx = futures[future]
                try:
                    result = future.result(timeout=900)
                    results.append({"task": idx, "success": True, "result": result})
                except Exception as e:
                    results.append({"task": idx, "success": False, "error": str(e)})
        
        return sorted(results, key=lambda r: r["task"])
    
    def tiered_execute(self, prompt: str, start_tier: int = 1, max_tier: int = 4) -> dict:
        """Execute with automatic tier escalation.
        
        Starts at start_tier, if the result seems incomplete or errors,
        escalates to the next tier with the previous tier's output as context.
        
        This is the "scaffold + finish" pattern:
        - Tier 1 (9B) scaffolds the structure
        - Tier 2 (32B) fills in the implementation
        - Tier 3 (72B) reviews and fixes
        - Tier 4 (235B) handles the hardest problems
        """
        context = ""
        last_result = ""
        
        for tier in range(start_tier, max_tier + 1):
            llm = self.get_llm(tier, fallback_up=False)
            if llm is None:
                continue
            
            # Build messages
            messages = [{"role": "system", "content": f"You are a tier-{tier} AI assistant."}]
            
            if context:
                messages.append({
                    "role": "user",
                    "content": f"A previous (smaller) model produced this partial result:\n\n{context}\n\nPlease improve, complete, and fix any issues. Original task:\n\n{prompt}",
                })
            else:
                messages.append({"role": "user", "content": prompt})
            
            try:
                response = llm.call(messages)
                last_result = response.get("content", "")
                
                # Simple quality check — if response is substantial, we're done
                if len(last_result) > 200 and "error" not in last_result.lower()[:50]:
                    return {
                        "success": True,
                        "tier": tier,
                        "model": llm.model,
                        "result": last_result,
                    }
                
                # Short/error response — escalate with this as context
                context = last_result
                
            except RuntimeError:
                continue
        
        return {
            "success": bool(last_result),
            "tier": max_tier,
            "model": "escalated",
            "result": last_result or "All tiers failed",
        }
    
    def status(self) -> dict:
        """Get fleet status — all endpoints with health + busy state."""
        status = {}
        for ep in self.endpoints:
            self.check_health(ep)
            if ep.healthy:
                self.check_busy(ep)
            
            key = f"{ep.node}/{ep.name}"
            status[key] = {
                "node": ep.node,
                "model": ep.name,
                "tier": ep.tier,
                "url": ep.url,
                "healthy": ep.healthy,
                "busy": ep.busy,
            }
        return status
