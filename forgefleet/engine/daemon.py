"""ForgeFleet Daemon — background service for continuous fleet monitoring.

Runs as a persistent background process that:
1. Heartbeat check (every 30s) — pings all known endpoints for health/busy
2. Discovery scan (every 5min) — scans subnet for NEW LLM endpoints
3. Cluster health (every 60s) — monitors running clusters, auto-repairs
4. Event callbacks — notifies when nodes join/leave/fail

Also listens on a local UDP port for node announcements:
- New nodes broadcast "FORGEFLEET_ANNOUNCE:{port}" on startup
- Daemon immediately scans that IP and adds it to the fleet

Start: python3 -m forgefleet.engine.daemon
Stop: kill the process or send SIGTERM
"""
import json
import os
import signal
import socket
import sys
import time
import threading
from dataclasses import dataclass, field
from typing import Callable

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

from forgefleet.engine.discovery import NetworkDiscovery, DiscoveredEndpoint, TIER_NAMES
from forgefleet.engine.cluster import ClusterManager


ANNOUNCE_PORT = 50099  # UDP port for node announcements
HEARTBEAT_INTERVAL = 30  # seconds between health checks
DISCOVERY_INTERVAL = 300  # seconds between full subnet scans
CLUSTER_CHECK_INTERVAL = 60  # seconds between cluster health checks


@dataclass
class FleetEvent:
    """An event from the fleet."""
    type: str  # "node_joined", "node_left", "node_failed", "node_recovered", "cluster_degraded"
    node_ip: str = ""
    node_name: str = ""
    model_name: str = ""
    tier: int = 0
    details: str = ""
    timestamp: float = 0
    
    def __post_init__(self):
        if not self.timestamp:
            self.timestamp = time.time()


@dataclass
class FleetDaemon:
    """Background daemon for continuous fleet monitoring and discovery.
    
    Runs three loops:
    1. Heartbeat — fast health checks of known endpoints
    2. Discovery — periodic subnet scans for new nodes
    3. Listener — UDP listener for node announcements
    
    Plus event callbacks for integration with OpenClaw/MC.
    """
    discovery: NetworkDiscovery = field(default_factory=NetworkDiscovery)
    cluster_mgr: ClusterManager = field(default_factory=ClusterManager)
    callbacks: list = field(default_factory=list)  # list of Callable[[FleetEvent], None]
    
    # State
    known_endpoints: dict = field(default_factory=dict)  # "ip:port" -> DiscoveredEndpoint
    events: list = field(default_factory=list)
    running: bool = False
    
    # Threads
    _heartbeat_thread: threading.Thread = None
    _discovery_thread: threading.Thread = None
    _listener_thread: threading.Thread = None
    _cluster_thread: threading.Thread = None
    
    def on_event(self, callback: Callable):
        """Register a callback for fleet events."""
        self.callbacks.append(callback)
    
    def _emit(self, event: FleetEvent):
        """Emit an event to all registered callbacks."""
        self.events.append(event)
        # Keep last 1000 events
        if len(self.events) > 1000:
            self.events = self.events[-500:]
        
        for cb in self.callbacks:
            try:
                cb(event)
            except Exception:
                pass
    
    def start(self):
        """Start all daemon threads."""
        self.running = True
        
        # Initial scan
        print("🔍 Initial fleet scan...")
        endpoints = self.discovery.scan_known_hosts()
        for ep in endpoints:
            key = f"{ep.ip}:{ep.port}"
            self.known_endpoints[key] = ep
        print(f"   Found {len(endpoints)} endpoints")
        
        # Start threads
        self._heartbeat_thread = threading.Thread(target=self._heartbeat_loop, daemon=True)
        self._heartbeat_thread.start()
        
        self._discovery_thread = threading.Thread(target=self._discovery_loop, daemon=True)
        self._discovery_thread.start()
        
        self._listener_thread = threading.Thread(target=self._announcement_listener, daemon=True)
        self._listener_thread.start()
        
        self._cluster_thread = threading.Thread(target=self._cluster_health_loop, daemon=True)
        self._cluster_thread.start()
        
        print(f"🤖 ForgeFleet daemon running")
        print(f"   Heartbeat: every {HEARTBEAT_INTERVAL}s")
        print(f"   Discovery: every {DISCOVERY_INTERVAL}s")
        print(f"   Cluster check: every {CLUSTER_CHECK_INTERVAL}s")
        print(f"   Announcement listener: UDP :{ANNOUNCE_PORT}")
    
    def stop(self):
        """Stop all daemon threads."""
        self.running = False
        print("⛔ ForgeFleet daemon stopping...")
    
    def _heartbeat_loop(self):
        """Check health of all known endpoints periodically."""
        while self.running:
            time.sleep(HEARTBEAT_INTERVAL)
            
            for key, ep in list(self.known_endpoints.items()):
                was_healthy = ep.healthy
                
                # Quick health check
                try:
                    import urllib.request
                    req = urllib.request.Request(f"{ep.url}/health")
                    with urllib.request.urlopen(req, timeout=3) as resp:
                        data = json.loads(resp.read())
                        ep.healthy = data.get("status") == "ok"
                except Exception:
                    ep.healthy = False
                
                # Check busy status
                if ep.healthy:
                    try:
                        req = urllib.request.Request(f"{ep.url}/slots")
                        with urllib.request.urlopen(req, timeout=3) as resp:
                            slots = json.loads(resp.read())
                            if isinstance(slots, list):
                                ep.slots_busy = sum(1 for s in slots if s.get("is_processing"))
                                ep.slots_total = len(slots)
                    except Exception:
                        pass
                
                # Emit events on state changes
                if was_healthy and not ep.healthy:
                    self._emit(FleetEvent(
                        type="node_failed",
                        node_ip=ep.ip,
                        node_name=ep.hostname,
                        model_name=ep.model_name,
                        tier=ep.tier,
                        details=f"{ep.model_name} on {ep.hostname} went down",
                    ))
                elif not was_healthy and ep.healthy:
                    self._emit(FleetEvent(
                        type="node_recovered",
                        node_ip=ep.ip,
                        node_name=ep.hostname,
                        model_name=ep.model_name,
                        tier=ep.tier,
                        details=f"{ep.model_name} on {ep.hostname} is back online",
                    ))
    
    def _discovery_loop(self):
        """Periodically scan for new LLM endpoints."""
        while self.running:
            time.sleep(DISCOVERY_INTERVAL)
            
            try:
                endpoints = self.discovery.scan_known_hosts()
                
                for ep in endpoints:
                    key = f"{ep.ip}:{ep.port}"
                    if key not in self.known_endpoints:
                        # New endpoint discovered!
                        self.known_endpoints[key] = ep
                        tier_name = TIER_NAMES.get(ep.tier, "unknown")
                        
                        self._emit(FleetEvent(
                            type="node_joined",
                            node_ip=ep.ip,
                            node_name=ep.hostname,
                            model_name=ep.model_name,
                            tier=ep.tier,
                            details=f"New: {ep.model_name} (T{ep.tier} {tier_name}) @ {ep.url}",
                        ))
                
                # Check for disappeared endpoints
                current_keys = {f"{ep.ip}:{ep.port}" for ep in endpoints}
                for key in list(self.known_endpoints.keys()):
                    if key not in current_keys:
                        ep = self.known_endpoints[key]
                        if ep.healthy:  # Was healthy, now gone
                            ep.healthy = False
                            self._emit(FleetEvent(
                                type="node_left",
                                node_ip=ep.ip,
                                node_name=ep.hostname,
                                model_name=ep.model_name,
                                details=f"Disappeared: {ep.model_name} on {ep.hostname}",
                            ))
            except Exception:
                pass
    
    def _announcement_listener(self):
        """Listen for UDP announcements from new nodes.
        
        Protocol: new node sends UDP packet to broadcast:50099
        Payload: "FORGEFLEET_ANNOUNCE:{port}" (e.g., "FORGEFLEET_ANNOUNCE:51803")
        
        Daemon immediately scans the sender IP on that port.
        """
        try:
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind(("0.0.0.0", ANNOUNCE_PORT))
            sock.settimeout(5)  # Allow checking self.running periodically
        except Exception as e:
            print(f"  ⚠️ Announcement listener failed to bind: {e}")
            return
        
        while self.running:
            try:
                data, addr = sock.recvfrom(1024)
                message = data.decode().strip()
                sender_ip = addr[0]
                
                if message.startswith("FORGEFLEET_ANNOUNCE:"):
                    port_str = message.split(":")[1]
                    port = int(port_str)
                    
                    print(f"📣 Announcement from {sender_ip}:{port}")
                    
                    # Immediately scan this endpoint
                    ep = self.discovery.scan_port(sender_ip, port)
                    if ep:
                        key = f"{ep.ip}:{ep.port}"
                        is_new = key not in self.known_endpoints
                        self.known_endpoints[key] = ep
                        
                        if is_new:
                            self._emit(FleetEvent(
                                type="node_joined",
                                node_ip=ep.ip,
                                node_name=ep.hostname,
                                model_name=ep.model_name,
                                tier=ep.tier,
                                details=f"Announced: {ep.model_name} (T{ep.tier}) @ {ep.url}",
                            ))
                            print(f"  ✅ Added: {ep.model_name} @ {ep.url}")
                        else:
                            print(f"  ℹ️ Already known: {ep.model_name} @ {ep.url}")
                
            except socket.timeout:
                continue
            except Exception:
                continue
        
        sock.close()
    
    def _cluster_health_loop(self):
        """Monitor cluster health and auto-repair."""
        while self.running:
            time.sleep(CLUSTER_CHECK_INTERVAL)
            
            for name, cluster in list(self.cluster_mgr.clusters.items()):
                try:
                    health = self.cluster_mgr.health_check(name)
                    
                    if health.get("status") == "degraded":
                        self._emit(FleetEvent(
                            type="cluster_degraded",
                            details=f"Cluster '{name}' degraded — attempting repair",
                        ))
                        
                        # Auto-repair
                        repair = self.cluster_mgr.repair_cluster(name)
                        for r in repair.get("repaired", []):
                            status = "✅" if r["success"] else "❌"
                            print(f"  {status} Repaired {r['node']} in cluster '{name}'")
                    
                    elif health.get("status") == "failed":
                        self._emit(FleetEvent(
                            type="cluster_degraded",
                            details=f"Cluster '{name}' FAILED — master down",
                        ))
                except Exception:
                    pass
    
    def status(self) -> dict:
        """Get current daemon status."""
        by_tier = {}
        for ep in self.known_endpoints.values():
            by_tier.setdefault(ep.tier, []).append(ep)
        
        return {
            "running": self.running,
            "endpoints": len(self.known_endpoints),
            "healthy": sum(1 for ep in self.known_endpoints.values() if ep.healthy),
            "busy": sum(1 for ep in self.known_endpoints.values() if ep.slots_busy > 0),
            "tiers": {
                tier: {
                    "count": len(eps),
                    "healthy": sum(1 for e in eps if e.healthy),
                    "name": TIER_NAMES.get(tier, "?"),
                }
                for tier, eps in sorted(by_tier.items())
            },
            "clusters": len(self.cluster_mgr.clusters),
            "recent_events": [
                {"type": e.type, "details": e.details, "time": e.timestamp}
                for e in self.events[-10:]
            ],
        }


def announce_to_fleet(port: int, broadcast_ip: str = "255.255.255.255"):
    """Send an announcement to the ForgeFleet daemon.
    
    Call this when starting a new llama-server to notify ForgeFleet immediately.
    
    Usage on a new node:
        python3 -c "from forgefleet.engine.daemon import announce_to_fleet; announce_to_fleet(8081)"
    
    Or simpler:
        echo "FORGEFLEET_ANNOUNCE:51803" | nc -u -w1 255.255.255.255 50099
    """
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        message = f"FORGEFLEET_ANNOUNCE:{port}".encode()
        sock.sendto(message, (broadcast_ip, ANNOUNCE_PORT))
        sock.close()
        print(f"📣 Announced port {port} to ForgeFleet daemon")
    except Exception as e:
        print(f"⚠️ Announcement failed: {e}")


# ─── Main entry point ──────────────────────────────────

if __name__ == "__main__":
    daemon = FleetDaemon()
    
    # Register a simple print callback
    def print_event(event: FleetEvent):
        icons = {
            "node_joined": "🟢",
            "node_left": "🔴",
            "node_failed": "❌",
            "node_recovered": "✅",
            "cluster_degraded": "⚠️",
        }
        icon = icons.get(event.type, "📋")
        print(f"  {icon} [{event.type}] {event.details}")
    
    daemon.on_event(print_event)
    
    # Handle SIGTERM/SIGINT gracefully
    def shutdown(signum, frame):
        daemon.stop()
        sys.exit(0)
    
    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)
    
    daemon.start()
    
    # Keep main thread alive
    try:
        while daemon.running:
            time.sleep(1)
    except KeyboardInterrupt:
        daemon.stop()
