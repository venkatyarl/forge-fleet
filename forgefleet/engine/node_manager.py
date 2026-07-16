"""Node Manager — each node manages itself + communicates with peers.

Every node runs this to:
1. Monitor its own LLMs → restart if crashed
2. Monitor its own Docker containers → restart if crashed
3. Report status to the gateway (Taylor) via HTTP
4. If gateway is down → operate independently
5. Accept status queries from other nodes
"""
import json
import os
import socket
import subprocess
import time
import threading
from http.server import HTTPServer, BaseHTTPRequestHandler
from dataclasses import dataclass, field


@dataclass
class NodeStatus:
    """Status of this node."""
    name: str
    ip: str
    llms: list = field(default_factory=list)  # [{port, model, healthy, busy}]
    docker: list = field(default_factory=list)  # [{name, running}]
    agent_running: bool = False
    build_stats: dict = field(default_factory=dict)  # {completed, failed}
    timestamp: float = 0
    
    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "ip": self.ip,
            "llms": self.llms,
            "docker": self.docker,
            "agent_running": self.agent_running,
            "build_stats": self.build_stats,
            "timestamp": self.timestamp,
        }


class NodeManager:
    """Self-management for each fleet node.
    
    Runs on EVERY node (not just Taylor).
    Monitors local services, reports to gateway, accepts peer queries.
    """
    
    def __init__(self):
        from forgefleet import config
        self.config = config
        self.node_name = config.get_node_name()
        self.local_ip = config.get_local_ip()
        self.llm_ports = config.get_llm_ports()
        self.gateway_ip = config.get_node_ip(config.get_gateway_node())
        self.peers = {name: node.get("ip", "") for name, node in config.get_nodes().items()}
        self._status = NodeStatus(name=self.node_name, ip=self.local_ip)
    
    def check_local_llms(self) -> list[dict]:
        """Check LLMs running on THIS node."""
        import urllib.request
        
        llms = []
        for port in self.llm_ports:
            try:
                req = urllib.request.Request(f"http://localhost:{port}/health")
                with urllib.request.urlopen(req, timeout=3) as resp:
                    data = json.loads(resp.read())
                    healthy = data.get("status") == "ok"
                    
                    # Get model name
                    model = "unknown"
                    try:
                        req2 = urllib.request.Request(f"http://localhost:{port}/slots")
                        with urllib.request.urlopen(req2, timeout=3) as resp2:
                            slots = json.loads(resp2.read())
                            if slots:
                                model = slots[0].get("model", "unknown")
                    except:
                        pass
                    
                    llms.append({"port": port, "model": model, "healthy": healthy, "busy": False})
            except:
                pass  # Port not running on this node — skip
        
        self._status.llms = llms
        return llms
    
    def check_local_docker(self) -> list[dict]:
        """Check Docker containers on THIS node."""
        containers = []
        try:
            r = subprocess.run(
                ["docker", "ps", "-a", "--format", "{{.Names}}\t{{.Status}}"],
                capture_output=True, text=True, timeout=10,
            )
            for line in r.stdout.strip().split("\n"):
                if not line.strip():
                    continue
                parts = line.split("\t")
                name = parts[0]
                running = "Up" in (parts[1] if len(parts) > 1 else "")
                containers.append({"name": name, "running": running})
        except:
            pass
        
        self._status.docker = containers
        return containers
    
    def restart_local_llm(self, port: int) -> bool:
        """Restart an LLM on this node."""
        # Find model path from known locations
        model_paths = {
            51803: "models/qwen3.5-9b/Qwen3.5-9B-Q4_K_M.gguf",
            51802: "models/qwen2.5-coder-32b/Qwen2.5-Coder-32B-Instruct-Q4_K_M.gguf",
            51801: "models/qwen2.5-72b/Qwen2.5-72B-Instruct-Q4_K_M.gguf",
        }
        
        model = model_paths.get(port)
        if not model:
            return False
        
        home = os.path.expanduser("~")
        model_path = os.path.join(home, model)
        
        if not os.path.exists(model_path):
            return False
        
        # Kill existing
        subprocess.run(f"pkill -f 'llama-server.*{port}'", shell=True, capture_output=True)
        time.sleep(2)
        
        # Determine ctx size based on RAM
        node_info = self.config.get_node(self.node_name)
        ram = node_info.get("ram_gb", 16)
        if ram >= 96:
            ctx = 32768
        elif ram >= 64:
            ctx = 16384
        else:
            ctx = 8192
        
        # Start
        subprocess.Popen(
            f"nohup llama-server --model {model_path} --port {port} --host 0.0.0.0 "
            f"--ctx-size {ctx} --n-gpu-layers 99 --threads 4 > /tmp/llama-{port}.log 2>&1 &",
            shell=True,
        )
        
        time.sleep(5)
        
        # Verify
        try:
            import urllib.request
            req = urllib.request.Request(f"http://localhost:{port}/health")
            with urllib.request.urlopen(req, timeout=5) as resp:
                return json.loads(resp.read()).get("status") == "ok"
        except:
            return False
    
    def restart_local_docker(self, container_name: str) -> bool:
        """Restart a Docker container on this node."""
        try:
            r = subprocess.run(
                ["docker", "restart", container_name],
                capture_output=True, text=True, timeout=30,
            )
            return r.returncode == 0
        except:
            return False
    
    def self_heal(self) -> list[str]:
        """Check everything and fix what's broken."""
        fixed = []
        
        # Check + fix LLMs
        llms = self.check_local_llms()
        for llm in llms:
            if not llm["healthy"]:
                print(f"[{self.node_name}] LLM on port {llm['port']} is DOWN — restarting", flush=True)
                if self.restart_local_llm(llm["port"]):
                    fixed.append(f"LLM:{llm['port']}")
        
        # Check + fix Docker
        containers = self.check_local_docker()
        for c in containers:
            if not c["running"]:
                print(f"[{self.node_name}] Docker {c['name']} is DOWN — restarting", flush=True)
                if self.restart_local_docker(c["name"]):
                    fixed.append(f"Docker:{c['name']}")
        
        return fixed
    
    def report_to_gateway(self):
        """Send this node's status to the gateway."""
        self._status.timestamp = time.time()
        
        try:
            import urllib.request
            data = json.dumps(self._status.to_dict()).encode()
            req = urllib.request.Request(
                f"http://{self.gateway_ip}:51820/api/node-status",
                data=data,
                headers={"Content-Type": "application/json"},
            )
            urllib.request.urlopen(req, timeout=5)
        except:
            pass  # Gateway might be down — that's OK, keep working
    
    def query_peer(self, peer_name: str) -> dict:
        """Query another node's status."""
        peer_ip = self.peers.get(peer_name, "")
        if not peer_ip:
            return {}
        
        try:
            import urllib.request
            req = urllib.request.Request(f"http://{peer_ip}:51820/api/status")
            with urllib.request.urlopen(req, timeout=5) as resp:
                return json.loads(resp.read())
        except:
            return {}
    
    def get_status(self) -> dict:
        """Get this node's current status."""
        self.check_local_llms()
        self.check_local_docker()
        self._status.agent_running = self._check_agent_running()
        self._status.timestamp = time.time()
        return self._status.to_dict()
    
    def _check_agent_running(self) -> bool:
        """Check if the ForgeFleet sub-agent is running."""
        try:
            r = subprocess.run(
                ["ps", "aux"], capture_output=True, text=True, timeout=5,
            )
            return "forgefleet_subagent" in r.stdout or "AutonomousWorker" in r.stdout
        except:
            return False
    
    def run_monitor(self, interval: int = 60):
        """Continuous monitoring loop — check and heal every N seconds."""
        print(f"[{self.node_name}] Node manager starting (heal every {interval}s)", flush=True)
        
        while True:
            try:
                fixed = self.self_heal()
                if fixed:
                    print(f"[{self.node_name}] Fixed: {fixed}", flush=True)
                
                self.report_to_gateway()
                
            except Exception as e:
                print(f"[{self.node_name}] Monitor error: {e}", flush=True)
            
            time.sleep(interval)
