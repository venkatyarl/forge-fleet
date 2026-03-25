"""ForgeFleet Unified Server — daemon + MCP + autonomous worker in ONE process.

Runs independently. No OpenClaw needed. No Claude needed.
One process that:
1. Monitors the fleet (health, discovery, cluster)
2. Serves MCP tools (for OpenClaw/Claude when they ARE available)
3. Builds autonomously (claims tickets, runs crews, pushes code)
4. Serves the web dashboard
5. Exports Prometheus metrics

Start: python3 -m forgefleet.server
Or: forgefleet-server (if installed as package)
"""
import json
import os
import signal
import sys
import threading
import time
from http.server import HTTPServer, BaseHTTPRequestHandler

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from forgefleet.engine.daemon import FleetDaemon
from forgefleet.engine.autonomous import AutonomousWorker
from forgefleet.engine.dashboard import DASHBOARD_HTML
from forgefleet.engine.metrics import get_metrics
from forgefleet.engine.discovery import TIER_NAMES
from forgefleet.engine.mc_client import MCClient


# ─── HTTP Server (Dashboard + MCP + Metrics) ───────────

class UnifiedHandler(BaseHTTPRequestHandler):
    """Serves dashboard, MCP, metrics, and API on one port."""
    
    _daemon = None
    _worker = None
    
    def do_GET(self):
        if self.path == "/" or self.path == "/dashboard":
            self._serve_html(DASHBOARD_HTML)
        elif self.path == "/health":
            self._serve_json({"status": "ok", "service": "forgefleet"})
        elif self.path == "/api/status":
            self._serve_json(self._get_status())
        elif self.path == "/api/fleet":
            self._serve_json(self._get_fleet())
        elif self.path == "/api/tickets":
            mc = MCClient()
            self._serve_json(mc.stats())
        elif self.path == "/api/worker":
            if self._worker:
                self._serve_json(self._worker.status())
            else:
                self._serve_json({"status": "not running"})
        elif self.path == "/metrics":
            metrics = get_metrics()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(metrics.export_prometheus().encode())
        else:
            self.send_response(404)
            self.end_headers()
    
    def do_POST(self):
        if self.path == "/mcp":
            # Handle MCP JSON-RPC requests
            content_length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(content_length).decode()
            
            try:
                request = json.loads(body)
                response = self._handle_mcp(request)
                self._serve_json(response)
            except Exception as e:
                self._serve_json({
                    "jsonrpc": "2.0", "id": None,
                    "error": {"code": -32603, "message": str(e)},
                })
        else:
            self.send_response(404)
            self.end_headers()
    
    def _handle_mcp(self, request: dict) -> dict:
        """Handle MCP JSON-RPC request."""
        method = request.get("method", "")
        req_id = request.get("id")
        params = request.get("params", {})
        
        if method == "initialize":
            return {
                "jsonrpc": "2.0", "id": req_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "forgefleet", "version": "1.0.0"},
                },
            }
        
        elif method == "tools/list":
            return {
                "jsonrpc": "2.0", "id": req_id,
                "result": {"tools": self._get_tools_list()},
            }
        
        elif method == "tools/call":
            tool_name = params.get("name", "")
            args = params.get("arguments", {})
            result = self._call_tool(tool_name, args)
            return {
                "jsonrpc": "2.0", "id": req_id,
                "result": {"content": [{"type": "text", "text": result}]},
            }
        
        return {
            "jsonrpc": "2.0", "id": req_id,
            "error": {"code": -32601, "message": f"Unknown method: {method}"},
        }
    
    def _get_tools_list(self) -> list:
        return [
            {"name": "fleet_status", "description": "Get fleet health and model status",
             "inputSchema": {"type": "object", "properties": {}}},
            {"name": "fleet_scan", "description": "Scan for new LLM endpoints",
             "inputSchema": {"type": "object", "properties": {}}},
            {"name": "fleet_run", "description": "Run a prompt through tiered LLM pipeline",
             "inputSchema": {"type": "object", "properties": {"prompt": {"type": "string"}}, "required": ["prompt"]}},
            {"name": "worker_status", "description": "Get autonomous worker status",
             "inputSchema": {"type": "object", "properties": {}}},
            {"name": "worker_start", "description": "Start the autonomous worker",
             "inputSchema": {"type": "object", "properties": {"repo": {"type": "string"}}}},
            {"name": "worker_stop", "description": "Stop the autonomous worker",
             "inputSchema": {"type": "object", "properties": {}}},
        ]
    
    def _call_tool(self, name: str, args: dict) -> str:
        if name == "fleet_status":
            return json.dumps(self._get_fleet(), indent=2)
        elif name == "fleet_scan":
            if self._daemon:
                eps = self._daemon.discovery.scan_known_hosts()
                return f"Found {len(eps)} endpoints"
            return "Daemon not running"
        elif name == "fleet_run":
            from forgefleet.engine.fleet_router import FleetRouter
            router = FleetRouter()
            result = router.tiered_execute(args.get("prompt", ""), 1, 3)
            return result.get("result", "No result")
        elif name == "worker_status":
            if self._worker:
                return json.dumps(self._worker.status(), indent=2)
            return "Worker not running"
        elif name == "worker_start":
            return "Use the daemon — worker runs automatically when idle"
        elif name == "worker_stop":
            if self._worker:
                self._worker._running = False
                return "Worker stopping..."
            return "Worker not running"
        return f"Unknown tool: {name}"
    
    def _get_status(self) -> dict:
        status = {"service": "forgefleet", "uptime": "running"}
        if self._daemon:
            status.update(self._daemon.status())
        if self._worker:
            status["worker"] = self._worker.status()
        try:
            mc = MCClient()
            status["tickets"] = mc.stats()
        except:
            pass
        return status
    
    def _get_fleet(self) -> dict:
        if not self._daemon:
            return {"endpoints": []}
        
        endpoints = []
        for ep in self._daemon.known_endpoints.values():
            endpoints.append({
                "node": ep.hostname or ep.ip,
                "ip": ep.ip,
                "port": ep.port,
                "model": ep.model_name,
                "tier": ep.tier,
                "healthy": ep.healthy,
                "busy": ep.slots_busy > 0 if hasattr(ep, "slots_busy") else False,
                "ctx_size": ep.ctx_size,
            })
        return {"endpoints": endpoints}
    
    def _serve_json(self, data: dict):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode())
    
    def _serve_html(self, html: str):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(html.encode())
    
    def log_message(self, format, *args):
        pass  # Suppress access logs


# ─── Main Server ────────────────────────────────────────

def main():
    port = int(os.environ.get("FORGEFLEET_PORT", "51820"))
    repo = os.environ.get("FORGEFLEET_REPO", "/Users/venkat/taylorProjects/HireFlow360")
    auto_work = os.environ.get("FORGEFLEET_AUTO_WORK", "true").lower() == "true"
    
    print(f"⚡ ForgeFleet Unified Server starting")
    print(f"   Port: {port}")
    print(f"   Repo: {repo}")
    print(f"   Auto-work: {auto_work}")
    
    # Start daemon (fleet monitoring)
    daemon = FleetDaemon()
    daemon.start()
    UnifiedHandler._daemon = daemon
    
    # Start autonomous worker (builds when idle)
    worker = None
    if auto_work:
        worker = AutonomousWorker(repo_dir=repo, only_when_idle=True)
        worker_thread = threading.Thread(target=worker.run, daemon=True)
        worker_thread.start()
        UnifiedHandler._worker = worker
        print(f"   Worker: autonomous mode (idle-only)")
    
    # Start HTTP server (dashboard + MCP + metrics)
    server = HTTPServer(("0.0.0.0", port), UnifiedHandler)
    print(f"   Dashboard: http://localhost:{port}")
    print(f"   MCP: http://localhost:{port}/mcp")
    print(f"   Metrics: http://localhost:{port}/metrics")
    print(f"   Health: http://localhost:{port}/health")
    print(f"\n🤖 ForgeFleet is live. OpenClaw optional.")
    
    # Graceful shutdown
    def shutdown(signum, frame):
        print("\n⛔ Shutting down...")
        daemon.stop()
        if worker:
            worker._running = False
        server.shutdown()
        sys.exit(0)
    
    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)
    
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        shutdown(None, None)


if __name__ == "__main__":
    main()
