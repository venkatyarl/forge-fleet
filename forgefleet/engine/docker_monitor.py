"""Docker Monitor — watch and restart containers as part of fleet management.

ForgeFleet manages EVERYTHING: LLMs, sub-agents, AND Docker containers.
If HireFlow backend dies, ForgeFleet restarts it. No human needed.
"""
import json
import os
import subprocess
import time
from dataclasses import dataclass, field


@dataclass
class ContainerStatus:
    name: str
    running: bool
    health: str  # "healthy", "unhealthy", "none", "starting"
    port: int = 0
    image: str = ""
    uptime: str = ""


class DockerMonitor:
    """Monitor and restart Docker containers across the fleet."""
    
    def __init__(self):
        from forgefleet import config
        self.services = config.get("services", {})
    
    def check_all(self) -> list[ContainerStatus]:
        """Check all Docker containers on this machine."""
        try:
            r = subprocess.run(
                ["docker", "ps", "-a", "--format", "{{.Names}}\t{{.Status}}\t{{.Ports}}\t{{.Image}}"],
                capture_output=True, text=True, timeout=10,
            )
            
            containers = []
            for line in r.stdout.strip().split("\n"):
                if not line.strip():
                    continue
                parts = line.split("\t")
                name = parts[0] if len(parts) > 0 else ""
                status_text = parts[1] if len(parts) > 1 else ""
                ports = parts[2] if len(parts) > 2 else ""
                image = parts[3] if len(parts) > 3 else ""
                
                running = "Up" in status_text
                health = "none"
                if "(healthy)" in status_text:
                    health = "healthy"
                elif "(unhealthy)" in status_text:
                    health = "unhealthy"
                
                # Extract port
                port = 0
                if ":" in ports:
                    try:
                        port = int(ports.split(":")[1].split("->")[0])
                    except:
                        pass
                
                containers.append(ContainerStatus(
                    name=name, running=running, health=health,
                    port=port, image=image, uptime=status_text,
                ))
            
            return containers
        except Exception:
            return []
    
    def check_service(self, service_name: str) -> bool:
        """Check if a specific service is healthy."""
        port = self.services.get(service_name, {}).get("port", 0)
        if not port:
            return True  # No port configured = skip
        
        try:
            import urllib.request
            req = urllib.request.Request(f"http://localhost:{port}/health")
            with urllib.request.urlopen(req, timeout=5) as resp:
                return resp.status == 200
        except:
            return False
    
    def restart_container(self, container_name: str) -> bool:
        """Restart a Docker container."""
        try:
            r = subprocess.run(
                ["docker", "restart", container_name],
                capture_output=True, text=True, timeout=30,
            )
            return r.returncode == 0
        except:
            return False
    
    def check_and_restart_all(self) -> list[str]:
        """Check all monitored services and restart any that are down."""
        restarted = []
        
        # Map service names to container names
        service_containers = {
            "mc": "mc_backend",
            "hireflow_backend": "hireflow-backend",
        }
        
        for service, container in service_containers.items():
            if not self.check_service(service):
                print(f"  ⚠️ {service} is DOWN — restarting {container}...", flush=True)
                if self.restart_container(container):
                    time.sleep(5)
                    if self.check_service(service):
                        restarted.append(f"{container} (was down, now UP)")
                        print(f"  ✅ {container} restarted successfully", flush=True)
                    else:
                        restarted.append(f"{container} (restart attempted, still down)")
                        print(f"  ❌ {container} still down after restart", flush=True)
                else:
                    print(f"  ❌ Failed to restart {container}", flush=True)
        
        return restarted
    
    def remote_check(self, node_ip: str, ssh_user: str = "") -> list[ContainerStatus]:
        """Check Docker containers on a remote node."""
        target = f"{ssh_user}@{node_ip}" if ssh_user else node_ip
        try:
            r = subprocess.run(
                ["ssh", "-o", "ConnectTimeout=5", target,
                 "docker ps -a --format '{{.Names}}\t{{.Status}}'"],
                capture_output=True, text=True, timeout=10,
            )
            
            containers = []
            for line in r.stdout.strip().split("\n"):
                if not line.strip():
                    continue
                parts = line.split("\t")
                name = parts[0] if parts else ""
                status_text = parts[1] if len(parts) > 1 else ""
                running = "Up" in status_text
                
                containers.append(ContainerStatus(
                    name=name, running=running, health="none",
                    uptime=status_text,
                ))
            
            return containers
        except:
            return []
    
    def status_report(self) -> str:
        """Generate a Docker status report."""
        containers = self.check_all()
        if not containers:
            return "No Docker containers found"
        
        lines = ["Docker containers:"]
        for c in containers:
            icon = "🟢" if c.running else "🔴"
            lines.append(f"  {icon} {c.name}: {c.uptime}")
        
        return "\n".join(lines)
