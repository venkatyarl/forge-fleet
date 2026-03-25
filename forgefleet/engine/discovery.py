"""Network Discovery — auto-discover LLM endpoints on the local network.

Scans the local subnet for llama.cpp / Ollama / vLLM servers.
No fleet.json needed — finds everything automatically.
Also handles model installation and updates on new nodes.
"""
import json
import os
import socket
import subprocess
import time
import urllib.request
import urllib.error
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from typing import Optional


# ─── Per-tier timeout configuration ────────────────────

TIER_TIMEOUTS = {
    1: 120,    # 9B models — fast, 2 min max
    2: 300,    # 32B models — moderate, 5 min
    3: 600,    # 72B models — complex, 10 min
    4: 900,    # 235B+ models — expert, 15 min
}

TIER_NAMES = {
    1: "fast (9B)",
    2: "code (32B)", 
    3: "review (72B)",
    4: "expert (235B+)",
}


@dataclass
class DiscoveredEndpoint:
    """A model endpoint discovered on the network."""
    ip: str
    port: int
    model_name: str = ""
    model_size: str = ""
    tier: int = 0
    slots_total: int = 0
    slots_busy: int = 0
    ctx_size: int = 0
    hostname: str = ""
    url: str = ""
    timeout: int = 120
    
    def __post_init__(self):
        self.url = f"http://{self.ip}:{self.port}"
        if self.tier:
            self.timeout = TIER_TIMEOUTS.get(self.tier, 300)


@dataclass 
class NetworkDiscovery:
    """Discovers LLM servers on the local network automatically.
    
    Scan methods:
    1. Known ports scan (8080-8083) on local subnet
    2. Fleet.json augmentation (if available)
    3. mDNS/Bonjour discovery (future)
    
    When a new endpoint is found:
    - Queries /health, /slots, /v1/models for capabilities
    - Auto-classifies tier based on model size
    - Stores in discovered_endpoints
    """
    subnet: str = ""
    known_ports: list = field(default_factory=lambda: [8080, 8081, 8082, 8083])
    discovered: list = field(default_factory=list)
    scan_timeout: float = 1.0  # TCP connect timeout per IP
    
    def __post_init__(self):
        if not self.subnet:
            self.subnet = self._detect_subnet()
    
    def _detect_subnet(self) -> str:
        """Detect the local subnet from network interfaces."""
        try:
            # Get local IP
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            local_ip = s.getsockname()[0]
            s.close()
            # Return /24 subnet
            parts = local_ip.split(".")
            return f"{parts[0]}.{parts[1]}.{parts[2]}"
        except Exception:
            return "192.168.5"  # Fallback to our known subnet
    
    def scan_port(self, ip: str, port: int) -> Optional[DiscoveredEndpoint]:
        """Check if an LLM server is running at ip:port."""
        # Quick TCP connect check
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(self.scan_timeout)
        try:
            result = sock.connect_ex((ip, port))
            if result != 0:
                return None
        finally:
            sock.close()
        
        # TCP open — check if it's actually an LLM server
        endpoint = DiscoveredEndpoint(ip=ip, port=port)
        
        # Try /health endpoint (llama.cpp)
        try:
            req = urllib.request.Request(f"{endpoint.url}/health")
            with urllib.request.urlopen(req, timeout=3) as resp:
                data = json.loads(resp.read())
                if data.get("status") != "ok":
                    return None
        except Exception:
            return None
        
        # Get model info from /slots
        try:
            req = urllib.request.Request(f"{endpoint.url}/slots")
            with urllib.request.urlopen(req, timeout=3) as resp:
                slots = json.loads(resp.read())
                if isinstance(slots, list) and slots:
                    slot = slots[0]
                    endpoint.model_name = slot.get("model", "unknown")
                    endpoint.ctx_size = slot.get("n_ctx", 0)
                    endpoint.slots_total = len(slots)
                    endpoint.slots_busy = sum(
                        1 for s in slots if s.get("is_processing", False)
                    )
        except Exception:
            pass
        
        # Get model info from /v1/models (OpenAI-compatible)
        if not endpoint.model_name or endpoint.model_name == "unknown":
            try:
                req = urllib.request.Request(f"{endpoint.url}/v1/models")
                with urllib.request.urlopen(req, timeout=3) as resp:
                    data = json.loads(resp.read())
                    models = data.get("data", [])
                    if models:
                        endpoint.model_name = models[0].get("id", "unknown")
            except Exception:
                pass
        
        # Get hostname via reverse DNS or SSH
        try:
            hostname = socket.gethostbyaddr(ip)[0]
            endpoint.hostname = hostname.split(".")[0]
        except Exception:
            endpoint.hostname = ip
        
        # Classify tier based on model name
        endpoint.tier = self._classify_tier(endpoint.model_name)
        endpoint.timeout = TIER_TIMEOUTS.get(endpoint.tier, 300)
        
        return endpoint
    
    def _classify_tier(self, model_name: str) -> int:
        """Classify model tier from its name/size."""
        name = model_name.lower()
        
        # Check for size indicators
        if any(s in name for s in ["235b", "397b", "405b", "671b", "moe"]):
            return 4
        if any(s in name for s in ["70b", "72b", "65b"]):
            return 3
        if any(s in name for s in ["32b", "34b", "27b", "22b"]):
            return 2
        if any(s in name for s in ["9b", "8b", "7b", "14b", "3b", "1b"]):
            return 1
        
        # Default to tier 1 if can't determine
        return 1
    
    def scan_subnet(self, ip_range: range = None) -> list[DiscoveredEndpoint]:
        """Scan the entire subnet for LLM servers.
        
        Scans known_ports on each IP in the subnet using thread pool.
        Returns all discovered endpoints.
        """
        if ip_range is None:
            ip_range = range(1, 255)
        
        targets = []
        for last_octet in ip_range:
            ip = f"{self.subnet}.{last_octet}"
            for port in self.known_ports:
                targets.append((ip, port))
        
        print(f"🔍 Scanning {len(targets)} targets on {self.subnet}.0/24...")
        self.discovered = []
        
        with ThreadPoolExecutor(max_workers=50) as executor:
            futures = {
                executor.submit(self.scan_port, ip, port): (ip, port)
                for ip, port in targets
            }
            
            for future in as_completed(futures):
                result = future.result()
                if result:
                    self.discovered.append(result)
                    tier_name = TIER_NAMES.get(result.tier, "unknown")
                    print(f"  🟢 Found: {result.hostname}/{result.model_name} "
                          f"(T{result.tier} {tier_name}) @ {result.url} "
                          f"[ctx:{result.ctx_size}, slots:{result.slots_total}]")
        
        print(f"✅ Scan complete: {len(self.discovered)} LLM endpoints found")
        return self.discovered
    
    def scan_known_hosts(self, hosts: list[str] = None) -> list[DiscoveredEndpoint]:
        """Quick scan of known hosts only (faster than full subnet scan).
        
        If no hosts provided, reads from fleet.json or uses defaults.
        """
        if hosts is None:
            hosts = self._get_known_ips()
        
        targets = []
        for ip in hosts:
            for port in self.known_ports:
                targets.append((ip, port))
        
        self.discovered = []
        
        with ThreadPoolExecutor(max_workers=20) as executor:
            futures = {
                executor.submit(self.scan_port, ip, port): (ip, port)
                for ip, port in targets
            }
            
            for future in as_completed(futures):
                result = future.result()
                if result:
                    self.discovered.append(result)
        
        return self.discovered
    
    def _get_known_ips(self) -> list[str]:
        """Get known IPs from fleet.json or defaults."""
        for path in [
            os.path.expanduser("~/fleet.json"),
            os.path.expanduser("~/.openclaw/workspace/fleet.json"),
        ]:
            if os.path.exists(path):
                try:
                    with open(path) as f:
                        fleet = json.load(f)
                    return [
                        node.get("ip", "")
                        for node in fleet.get("nodes", {}).values()
                        if node.get("ip")
                    ]
                except Exception:
                    pass
        
        # Default fleet IPs
        return [
            "192.168.5.100", "192.168.5.102", "192.168.5.103",
            "192.168.5.104", "192.168.5.106", "192.168.5.108",
        ]
    
    def check_model_status(self, ip: str) -> dict:
        """Check what models are available/running on a specific node.
        
        Returns info about installed models, running servers, available disk/RAM.
        Used to determine if we can install additional models.
        """
        info = {"ip": ip, "models_running": [], "can_install": False}
        
        # Check running models on known ports
        for port in self.known_ports:
            ep = self.scan_port(ip, port)
            if ep:
                info["models_running"].append({
                    "name": ep.model_name,
                    "port": port,
                    "tier": ep.tier,
                    "ctx_size": ep.ctx_size,
                })
        
        # Check available RAM via SSH (if accessible)
        try:
            r = subprocess.run(
                ["ssh", "-o", "ConnectTimeout=3", "-o", "StrictHostKeyChecking=no",
                 ip, "free -g 2>/dev/null || sysctl -n hw.memsize 2>/dev/null"],
                capture_output=True, text=True, timeout=5
            )
            if r.returncode == 0:
                info["ram_info"] = r.stdout.strip()[:200]
                info["can_install"] = True
        except Exception:
            pass
        
        return info
    
    def install_model(self, ip: str, model_url: str, model_path: str,
                      port: int = 8081, ctx_size: int = 8192) -> dict:
        """Install and start a model on a remote node.
        
        Steps:
        1. SSH to node
        2. Download model (wget/curl)
        3. Start llama-server
        4. Verify health
        
        Returns status dict.
        """
        result = {"ip": ip, "success": False, "steps": []}
        
        # Step 1: Check if model already exists
        try:
            r = subprocess.run(
                ["ssh", "-o", "ConnectTimeout=5", ip, f"ls -la {model_path}"],
                capture_output=True, text=True, timeout=10
            )
            if r.returncode == 0:
                result["steps"].append("Model file already exists")
            else:
                # Download model
                result["steps"].append(f"Downloading model to {model_path}...")
                r = subprocess.run(
                    ["ssh", ip, f"mkdir -p $(dirname {model_path}) && "
                     f"wget -q -O {model_path} '{model_url}'"],
                    capture_output=True, text=True, timeout=3600  # 1hr for large models
                )
                if r.returncode != 0:
                    result["steps"].append(f"Download failed: {r.stderr[:200]}")
                    return result
                result["steps"].append("Download complete")
        except Exception as e:
            result["steps"].append(f"SSH failed: {e}")
            return result
        
        # Step 2: Start llama-server
        try:
            cmd = (
                f"nohup llama-server "
                f"--model {model_path} "
                f"--port {port} "
                f"--host 0.0.0.0 "
                f"--ctx-size {ctx_size} "
                f"--n-gpu-layers 99 "
                f"--threads 4 "
                f"> /tmp/llama-{port}.log 2>&1 &"
            )
            subprocess.run(
                ["ssh", ip, cmd],
                capture_output=True, text=True, timeout=10
            )
            result["steps"].append(f"Started llama-server on port {port}")
        except Exception as e:
            result["steps"].append(f"Failed to start: {e}")
            return result
        
        # Step 3: Wait and verify
        time.sleep(5)
        ep = self.scan_port(ip, port)
        if ep:
            result["success"] = True
            result["steps"].append(f"Verified: {ep.model_name} running on {ip}:{port}")
            result["endpoint"] = {
                "model": ep.model_name, "tier": ep.tier,
                "url": ep.url, "ctx_size": ep.ctx_size,
            }
        else:
            result["steps"].append("Server started but health check failed")
        
        return result
    
    def update_fleet_json(self, fleet_path: str = None):
        """Update fleet.json with discovered endpoints.
        
        Merges discovered endpoints into existing fleet.json,
        adding new nodes and models without overwriting existing config.
        """
        if fleet_path is None:
            fleet_path = os.path.expanduser("~/.openclaw/workspace/fleet.json")
        
        if not os.path.exists(fleet_path):
            return
        
        with open(fleet_path) as f:
            fleet = json.load(f)
        
        # Group discovered endpoints by IP
        by_ip = {}
        for ep in self.discovered:
            if ep.ip not in by_ip:
                by_ip[ep.ip] = []
            by_ip[ep.ip].append(ep)
        
        # Merge into fleet.json
        for node_name, node in fleet.get("nodes", {}).items():
            node_ip = node.get("ip", "")
            if node_ip in by_ip:
                # Update llama_cpp.models with discovered info
                if "llama_cpp" not in node:
                    node["llama_cpp"] = {}
                
                discovered_models = []
                for ep in by_ip[node_ip]:
                    model_str = (
                        f"{ep.model_name} (port {ep.port}, "
                        f"ctx {ep.ctx_size}, tier {ep.tier}) — active"
                    )
                    discovered_models.append(model_str)
                
                node["llama_cpp"]["discovered_models"] = discovered_models
                node["llama_cpp"]["last_scan"] = time.strftime("%Y-%m-%d %H:%M:%S")
        
        with open(fleet_path, "w") as f:
            json.dump(fleet, f, indent=2)
