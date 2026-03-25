"""Cluster Manager — distribute large models across multiple nodes.

Handles llama.cpp RPC-based model sharding:
- Master node runs llama-server with --rpc flags pointing to workers
- Worker nodes run rpc-server sharing their GPU/RAM
- Auto-calculates how many nodes needed based on model size + node RAM
- Monitors cluster health and auto-restarts failed workers
- Can create, expand, shrink, and destroy clusters
"""
import json
import os
import subprocess
import time
import urllib.request
from dataclasses import dataclass, field
from concurrent.futures import ThreadPoolExecutor, as_completed
from typing import Optional


# Model size estimates (Q4_K_M quantization)
MODEL_SIZES_GB = {
    "9b": 6,
    "14b": 9,
    "32b": 20,
    "72b": 42,
    "235b": 135,
    "397b": 227,
    "405b": 230,
    "671b": 380,
}

# RAM overhead for OS + llama.cpp runtime
RAM_OVERHEAD_GB = 4


@dataclass
class ClusterNode:
    """A node participating in a cluster."""
    name: str
    ip: str
    ssh_user: str = ""
    ram_gb: int = 0
    role: str = ""  # "master" or "worker"
    rpc_port: int = 50052
    model_port: int = 8080
    healthy: bool = False
    pid: int = 0
    
    def ssh_target(self) -> str:
        if self.ssh_user:
            return f"{self.ssh_user}@{self.ip}"
        return self.ip


@dataclass
class Cluster:
    """A model cluster running across multiple nodes."""
    name: str
    model_name: str
    model_path: str
    model_size_gb: float
    master: ClusterNode = None
    workers: list = field(default_factory=list)
    master_port: int = 8080
    ctx_size: int = 8192
    ngl: int = 99  # GPU layers
    status: str = "stopped"  # stopped, starting, running, degraded, failed


@dataclass
class ClusterManager:
    """Manages model clusters across the fleet.
    
    Key operations:
    - plan_cluster(): given a model size, figure out which nodes to use
    - create_cluster(): start RPC workers + master
    - health_check(): verify all nodes are alive
    - repair_cluster(): restart failed workers
    - expand_cluster(): add a node to existing cluster
    - destroy_cluster(): stop everything
    """
    fleet_path: str = ""
    clusters: dict = field(default_factory=dict)  # name -> Cluster
    
    def __post_init__(self):
        if not self.fleet_path:
            for path in [
                os.path.expanduser("~/fleet.json"),
                os.path.expanduser("~/.openclaw/workspace/fleet.json"),
            ]:
                if os.path.exists(path):
                    self.fleet_path = path
                    break
    
    def get_fleet_nodes(self) -> list[ClusterNode]:
        """Load available nodes from fleet.json."""
        nodes = []
        if not self.fleet_path or not os.path.exists(self.fleet_path):
            return nodes
        
        with open(self.fleet_path) as f:
            fleet = json.load(f)
        
        for name, info in fleet.get("nodes", {}).items():
            nodes.append(ClusterNode(
                name=name,
                ip=info.get("ip", ""),
                ssh_user=info.get("ssh_user", name),
                ram_gb=info.get("ram_gb", 0),
            ))
        
        return nodes
    
    def estimate_model_size(self, model_name: str) -> float:
        """Estimate model size in GB from its name."""
        name_lower = model_name.lower()
        for size_key, gb in MODEL_SIZES_GB.items():
            if size_key in name_lower:
                return gb
        return 10  # Default estimate
    
    def plan_cluster(self, model_name: str, model_path: str,
                     master_name: str = "", exclude_nodes: list = None,
                     ) -> dict:
        """Plan which nodes to use for a model cluster.
        
        Algorithm:
        1. Estimate model size
        2. Check which nodes have enough combined RAM
        3. Pick the node with most RAM as master
        4. Add workers until we have enough RAM
        5. Return the plan
        
        Args:
            model_name: e.g., "Qwen3-235B"
            model_path: path to the GGUF file on master
            master_name: preferred master node (optional)
            exclude_nodes: nodes to skip
            
        Returns:
            Plan dict with master, workers, estimated performance
        """
        exclude = set(exclude_nodes or [])
        model_size = self.estimate_model_size(model_name)
        
        nodes = self.get_fleet_nodes()
        available = [n for n in nodes if n.name not in exclude and n.ram_gb > 0]
        
        if not available:
            return {"feasible": False, "reason": "No nodes available"}
        
        # Sort by RAM (descending) — biggest node = master
        available.sort(key=lambda n: n.ram_gb, reverse=True)
        
        # Check if single node can handle it
        biggest = available[0]
        usable_ram = biggest.ram_gb - RAM_OVERHEAD_GB
        
        if usable_ram >= model_size:
            return {
                "feasible": True,
                "type": "standalone",
                "model": model_name,
                "model_size_gb": model_size,
                "master": biggest.name,
                "workers": [],
                "total_ram": biggest.ram_gb,
                "note": f"Single node has enough RAM ({biggest.ram_gb}GB)",
            }
        
        # Need cluster — pick master (prefer specified, or biggest)
        if master_name:
            master = next((n for n in available if n.name == master_name), available[0])
        else:
            master = available[0]
        
        workers = []
        total_ram = master.ram_gb - RAM_OVERHEAD_GB
        
        for node in available:
            if node.name == master.name:
                continue
            if total_ram >= model_size:
                break
            workers.append(node)
            total_ram += node.ram_gb - RAM_OVERHEAD_GB
        
        if total_ram < model_size:
            return {
                "feasible": False,
                "reason": f"Not enough RAM. Need ~{model_size}GB, have {total_ram}GB usable across {len(workers) + 1} nodes",
                "model_size_gb": model_size,
                "available_ram": total_ram,
            }
        
        return {
            "feasible": True,
            "type": "cluster",
            "model": model_name,
            "model_size_gb": model_size,
            "master": master.name,
            "master_ip": master.ip,
            "master_ram": master.ram_gb,
            "workers": [{"name": w.name, "ip": w.ip, "ram": w.ram_gb} for w in workers],
            "total_ram": total_ram + RAM_OVERHEAD_GB * (len(workers) + 1),
            "usable_ram": total_ram,
            "rpc_port": 50052,
        }
    
    def create_cluster(self, plan: dict, model_path: str,
                       master_port: int = 8080, ctx_size: int = 8192,
                       ngl: int = 99) -> Cluster:
        """Create a cluster from a plan.
        
        Steps:
        1. Start RPC workers on all worker nodes
        2. Wait for workers to be ready
        3. Start master with --rpc flags pointing to workers
        4. Verify health
        """
        if not plan.get("feasible"):
            raise ValueError(f"Plan not feasible: {plan.get('reason')}")
        
        cluster = Cluster(
            name=plan["model"],
            model_name=plan["model"],
            model_path=model_path,
            model_size_gb=plan["model_size_gb"],
            master_port=master_port,
            ctx_size=ctx_size,
            ngl=ngl,
            status="starting",
        )
        
        if plan["type"] == "standalone":
            # Single node — just start llama-server
            master_ip = plan.get("master_ip", "")
            if not master_ip:
                nodes = self.get_fleet_nodes()
                master_node = next((n for n in nodes if n.name == plan["master"]), None)
                master_ip = master_node.ip if master_node else "127.0.0.1"
            
            cluster.master = ClusterNode(
                name=plan["master"], ip=master_ip,
                role="master", model_port=master_port,
            )
            
            self._start_standalone(cluster)
            return cluster
        
        # Cluster mode — start workers first, then master
        master_info = next(
            (n for n in self.get_fleet_nodes() if n.name == plan["master"]), None
        )
        
        cluster.master = ClusterNode(
            name=plan["master"],
            ip=plan.get("master_ip", master_info.ip if master_info else ""),
            ssh_user=master_info.ssh_user if master_info else "",
            ram_gb=plan.get("master_ram", 0),
            role="master",
            model_port=master_port,
        )
        
        # Start RPC workers in parallel
        print(f"🔧 Starting {len(plan['workers'])} RPC workers...")
        
        for w in plan["workers"]:
            fleet_node = next(
                (n for n in self.get_fleet_nodes() if n.name == w["name"]), None
            )
            worker = ClusterNode(
                name=w["name"], ip=w["ip"],
                ssh_user=fleet_node.ssh_user if fleet_node else w["name"],
                ram_gb=w["ram"], role="worker",
                rpc_port=plan.get("rpc_port", 50052),
            )
            cluster.workers.append(worker)
        
        # Start workers in parallel
        with ThreadPoolExecutor(max_workers=len(cluster.workers)) as executor:
            futures = {
                executor.submit(self._start_rpc_worker, worker): worker
                for worker in cluster.workers
            }
            for future in as_completed(futures):
                worker = futures[future]
                try:
                    success = future.result()
                    worker.healthy = success
                    icon = "✅" if success else "❌"
                    print(f"  {icon} RPC worker on {worker.name} ({worker.ip}:{worker.rpc_port})")
                except Exception as e:
                    worker.healthy = False
                    print(f"  ❌ RPC worker on {worker.name} failed: {e}")
        
        # Wait for workers to stabilize
        time.sleep(3)
        
        # Build RPC flags for master
        rpc_servers = ",".join(
            f"{w.ip}:{w.rpc_port}" for w in cluster.workers if w.healthy
        )
        
        if not rpc_servers:
            cluster.status = "failed"
            print("❌ No healthy RPC workers — cluster failed")
            return cluster
        
        # Start master
        print(f"🚀 Starting master on {cluster.master.name} with {len(cluster.workers)} RPC workers...")
        self._start_master(cluster, rpc_servers)
        
        # Verify
        time.sleep(5)
        if self._check_master_health(cluster):
            cluster.status = "running"
            print(f"✅ Cluster '{cluster.name}' running on port {master_port}")
        else:
            cluster.status = "degraded"
            print(f"⚠️ Cluster started but health check failed")
        
        self.clusters[cluster.name] = cluster
        return cluster
    
    def _start_rpc_worker(self, worker: ClusterNode) -> bool:
        """Start an RPC worker on a remote node."""
        # Kill existing rpc-server on the port
        kill_cmd = f"pkill -f 'rpc-server.*{worker.rpc_port}' 2>/dev/null; sleep 1"
        
        start_cmd = (
            f"nohup rpc-server "
            f"--host 0.0.0.0 "
            f"--port {worker.rpc_port} "
            f"> /tmp/rpc-{worker.rpc_port}.log 2>&1 &"
        )
        
        # Also try llama-rpc-server (different llama.cpp builds name it differently)
        fallback_cmd = (
            f"nohup llama-rpc-server "
            f"--host 0.0.0.0 "
            f"--port {worker.rpc_port} "
            f"> /tmp/rpc-{worker.rpc_port}.log 2>&1 &"
        )
        
        try:
            subprocess.run(
                ["ssh", "-o", "ConnectTimeout=5", worker.ssh_target(),
                 f"{kill_cmd} {start_cmd} || {fallback_cmd}"],
                capture_output=True, text=True, timeout=15
            )
            
            # Verify the port is open
            time.sleep(2)
            import socket
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.settimeout(3)
            result = sock.connect_ex((worker.ip, worker.rpc_port))
            sock.close()
            return result == 0
        except Exception:
            return False
    
    def _start_standalone(self, cluster: Cluster):
        """Start a standalone (non-clustered) model server."""
        cmd = (
            f"nohup llama-server "
            f"--model {cluster.model_path} "
            f"--port {cluster.master_port} "
            f"--host 0.0.0.0 "
            f"--ctx-size {cluster.ctx_size} "
            f"--n-gpu-layers {cluster.ngl} "
            f"--threads 4 "
            f"> /tmp/llama-{cluster.master_port}.log 2>&1 &"
        )
        
        try:
            if cluster.master.ip in ("127.0.0.1", "localhost") or \
               cluster.master.ip == self._get_local_ip():
                subprocess.run(cmd, shell=True, timeout=10)
            else:
                subprocess.run(
                    ["ssh", cluster.master.ssh_target(), cmd],
                    capture_output=True, text=True, timeout=10
                )
            
            time.sleep(5)
            if self._check_master_health(cluster):
                cluster.status = "running"
            else:
                cluster.status = "failed"
        except Exception as e:
            cluster.status = "failed"
    
    def _start_master(self, cluster: Cluster, rpc_servers: str):
        """Start the cluster master with RPC worker connections."""
        cmd = (
            f"nohup llama-server "
            f"--model {cluster.model_path} "
            f"--port {cluster.master_port} "
            f"--host 0.0.0.0 "
            f"--ctx-size {cluster.ctx_size} "
            f"--n-gpu-layers {cluster.ngl} "
            f"--rpc {rpc_servers} "
            f"--threads 4 "
            f"> /tmp/llama-cluster-{cluster.master_port}.log 2>&1 &"
        )
        
        try:
            if cluster.master.ip == self._get_local_ip():
                subprocess.run(cmd, shell=True, timeout=10)
            else:
                subprocess.run(
                    ["ssh", cluster.master.ssh_target(), cmd],
                    capture_output=True, text=True, timeout=10
                )
        except Exception:
            pass
    
    def _check_master_health(self, cluster: Cluster) -> bool:
        """Check if the cluster master is responding."""
        try:
            url = f"http://{cluster.master.ip}:{cluster.master_port}/health"
            req = urllib.request.Request(url)
            with urllib.request.urlopen(req, timeout=10) as resp:
                data = json.loads(resp.read())
                return data.get("status") == "ok"
        except Exception:
            return False
    
    def _get_local_ip(self) -> str:
        """Get local machine's IP."""
        import socket
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            ip = s.getsockname()[0]
            s.close()
            return ip
        except Exception:
            return "127.0.0.1"
    
    def health_check(self, cluster_name: str) -> dict:
        """Check health of all nodes in a cluster."""
        cluster = self.clusters.get(cluster_name)
        if not cluster:
            return {"error": f"Cluster '{cluster_name}' not found"}
        
        result = {
            "name": cluster_name,
            "model": cluster.model_name,
            "status": cluster.status,
            "master": {
                "name": cluster.master.name,
                "ip": cluster.master.ip,
                "port": cluster.master_port,
                "healthy": self._check_master_health(cluster),
            },
            "workers": [],
        }
        
        for worker in cluster.workers:
            import socket
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.settimeout(3)
            healthy = sock.connect_ex((worker.ip, worker.rpc_port)) == 0
            sock.close()
            
            result["workers"].append({
                "name": worker.name,
                "ip": worker.ip,
                "port": worker.rpc_port,
                "healthy": healthy,
            })
        
        # Update cluster status
        all_workers_ok = all(w["healthy"] for w in result["workers"])
        master_ok = result["master"]["healthy"]
        
        if master_ok and all_workers_ok:
            cluster.status = "running"
        elif master_ok:
            cluster.status = "degraded"
        else:
            cluster.status = "failed"
        
        result["status"] = cluster.status
        return result
    
    def repair_cluster(self, cluster_name: str) -> dict:
        """Restart any failed workers in a cluster."""
        cluster = self.clusters.get(cluster_name)
        if not cluster:
            return {"error": f"Cluster '{cluster_name}' not found"}
        
        repaired = []
        
        for worker in cluster.workers:
            import socket
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.settimeout(3)
            healthy = sock.connect_ex((worker.ip, worker.rpc_port)) == 0
            sock.close()
            
            if not healthy:
                print(f"🔧 Restarting RPC worker on {worker.name}...")
                success = self._start_rpc_worker(worker)
                worker.healthy = success
                repaired.append({
                    "node": worker.name,
                    "success": success,
                })
        
        # If master is down, restart it
        if not self._check_master_health(cluster):
            print(f"🔧 Restarting cluster master on {cluster.master.name}...")
            rpc_servers = ",".join(
                f"{w.ip}:{w.rpc_port}" for w in cluster.workers if w.healthy
            )
            if rpc_servers:
                self._start_master(cluster, rpc_servers)
                time.sleep(5)
                repaired.append({
                    "node": f"{cluster.master.name} (master)",
                    "success": self._check_master_health(cluster),
                })
        
        return {"repaired": repaired, "cluster_status": cluster.status}
    
    def destroy_cluster(self, cluster_name: str) -> dict:
        """Stop all nodes in a cluster."""
        cluster = self.clusters.get(cluster_name)
        if not cluster:
            return {"error": f"Cluster '{cluster_name}' not found"}
        
        stopped = []
        
        # Kill master
        try:
            subprocess.run(
                ["ssh", cluster.master.ssh_target(),
                 f"pkill -f 'llama-server.*{cluster.master_port}'"],
                capture_output=True, text=True, timeout=10
            )
            stopped.append(f"master ({cluster.master.name})")
        except Exception:
            pass
        
        # Kill workers
        for worker in cluster.workers:
            try:
                subprocess.run(
                    ["ssh", worker.ssh_target(),
                     f"pkill -f 'rpc-server.*{worker.rpc_port}'"],
                    capture_output=True, text=True, timeout=10
                )
                stopped.append(f"worker ({worker.name})")
            except Exception:
                pass
        
        cluster.status = "stopped"
        del self.clusters[cluster_name]
        
        return {"stopped": stopped}
