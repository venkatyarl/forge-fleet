"""Peer Mesh — nodes discover each other, elect degraded-mode coordinators, share state.

Three capabilities:
1. Peer Discovery: each node pings all others on port 51820
2. Degraded-Mode Operational Coordination: if Taylor is unavailable, another node can temporarily coordinate operations
3. Build Queue Sync: nodes announce what they're working on to avoid duplicates

Important: operational coordinator != canonical global governance writer.
Taylor remains the canonical writer for global governance state when available.
"""
import json
import os
import socket
import time
import threading
import urllib.request
import urllib.error
from dataclasses import dataclass, field


@dataclass
class PeerInfo:
    """Info about a peer node."""
    name: str
    ip: str
    healthy: bool = False
    last_seen: float = 0
    llm_count: int = 0
    agent_running: bool = False
    current_task: str = ""  # What ticket this node is building


class PeerMesh:
    """Distributed peer mesh — every node knows about every other node.
    
    Canonical governance writing and degraded-mode operational coordination are separate concepts.
    Each node:
    - Pings all peers every 30s
    - Knows who's alive, who's building what
    - May elect a degraded-mode operational coordinator when canonical primary is unavailable
    - Does not assume operational coordination implies canonical governance authority
    """
    
    def __init__(self):
        from forgefleet import config
        self.config = config
        self.node_name = config.get_node_name()
        self.local_ip = config.get_local_ip()
        self.peers: dict[str, PeerInfo] = {}
        self.current_task: str = ""  # What THIS node is building
        self.operational_coordinator: str = ""  # Temporary degraded-mode coordinator
        self.canonical_writer: str = config.get_canonical_writer()
        self._lock = threading.Lock()
        
        # Initialize peer list from config
        for name, node in config.get_nodes().items():
            if name != self.node_name:
                self.peers[name] = PeerInfo(
                    name=name,
                    ip=node.get("ip", ""),
                )
    
    def ping_all_peers(self) -> dict[str, bool]:
        """Ping all peers and update their status."""
        results = {}
        
        for name, peer in self.peers.items():
            try:
                req = urllib.request.Request(f"http://{peer.ip}:51820/api/status")
                with urllib.request.urlopen(req, timeout=5) as resp:
                    data = json.loads(resp.read())
                    
                    with self._lock:
                        peer.healthy = True
                        peer.last_seen = time.time()
                        peer.llm_count = len(data.get("llms", []))
                        peer.agent_running = data.get("agent_running", False)
                        peer.current_task = data.get("current_task", "")
                    
                    results[name] = True
            except:
                with self._lock:
                    peer.healthy = False
                results[name] = False
        
        return results
    
    def elect_coordinator(self) -> str:
        """Elect a degraded-mode operational coordinator — highest RAM node that's alive.
        
        If Taylor (gateway) is alive, it's always the operational coordinator.
        If Taylor is unavailable, the highest-RAM surviving node temporarily coordinates operations.
        This does NOT change canonical global governance writer semantics.
        """
        candidates = []
        
        # Add self
        my_ram = self.config.get_node(self.node_name).get("ram_gb", 0)
        my_role = self.config.get_node(self.node_name).get("role", "builder")
        candidates.append((self.node_name, my_ram, my_role))
        
        # Add alive peers
        for name, peer in self.peers.items():
            if peer.healthy:
                ram = self.config.get_node(name).get("ram_gb", 0)
                role = self.config.get_node(name).get("role", "builder")
                candidates.append((name, ram, role))
        
        # Gateway always wins if alive
        for name, ram, role in candidates:
            if role == "gateway":
                self.operational_coordinator = name
                return name
        
        # Otherwise highest RAM
        candidates.sort(key=lambda c: c[1], reverse=True)
        if candidates:
            self.operational_coordinator = candidates[0][0]
            return self.operational_coordinator
        
        return self.node_name  # Fallback to self
    
    def is_coordinator(self) -> bool:
        """Am I the current degraded-mode operational coordinator?"""
        return self.operational_coordinator == self.node_name

    def is_canonical_writer(self) -> bool:
        """Am I the canonical global governance writer?"""
        return self.canonical_writer == self.node_name
    
    def announce_task(self, ticket_id: str, title: str):
        """Announce what this node is working on — prevents duplicate claims."""
        self.current_task = f"{ticket_id}:{title[:50]}"
    
    def clear_task(self):
        """Clear task announcement — done or failed."""
        self.current_task = ""
    
    def get_claimed_tasks(self) -> set[str]:
        """Get all ticket IDs currently being worked on across the fleet."""
        claimed = set()
        
        # My task
        if self.current_task:
            tid = self.current_task.split(":")[0]
            claimed.add(tid)
        
        # Peer tasks
        for peer in self.peers.values():
            if peer.current_task and peer.healthy:
                tid = peer.current_task.split(":")[0]
                claimed.add(tid)
        
        return claimed
    
    def is_task_claimed(self, ticket_id: str) -> bool:
        """Check if any other node is already building this ticket."""
        for peer in self.peers.values():
            if peer.healthy and peer.current_task.startswith(ticket_id):
                return True
        return False
    
    def get_fleet_view(self) -> dict:
        """Get a complete view of the fleet from this node's perspective."""
        nodes = {}
        
        # Self
        nodes[self.node_name] = {
            "ip": self.local_ip,
            "healthy": True,
            "agent_running": True,
            "current_task": self.current_task,
            "is_coordinator": self.is_coordinator(),
        }
        
        # Peers
        for name, peer in self.peers.items():
            nodes[name] = {
                "ip": peer.ip,
                "healthy": peer.healthy,
                "last_seen": peer.last_seen,
                "agent_running": peer.agent_running,
                "current_task": peer.current_task,
                "is_coordinator": self.operational_coordinator == name,
            }
        
        return {
            "canonical_writer": self.canonical_writer,
            "coordinator": self.operational_coordinator,
            "is_canonical_writer": self.is_canonical_writer(),
            "nodes": nodes,
            "total_alive": sum(1 for n in nodes.values() if n["healthy"]),
            "total_building": sum(1 for n in nodes.values() if n.get("current_task")),
        }
    
    def run_mesh(self, interval: int = 30):
        """Continuous mesh loop — ping peers, elect coordinator, share state."""
        print(f"[{self.node_name}] Peer mesh starting ({len(self.peers)} peers)", flush=True)
        
        while True:
            try:
                # Ping all peers
                results = self.ping_all_peers()
                alive = sum(1 for v in results.values() if v)
                
                # Elect coordinator
                coordinator = self.elect_coordinator()
                
                if self.is_coordinator():
                    # Coordinator responsibilities (future: assign tasks, aggregate status)
                    pass
                
            except Exception as e:
                print(f"[{self.node_name}] Mesh error: {e}", flush=True)
            
            time.sleep(interval)
