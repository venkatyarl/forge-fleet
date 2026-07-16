"""ForgeFleet Node Agent — builds tickets + manages node + peer mesh + serves status."""
import sys
import os
import threading

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from forgefleet.engine.autonomous import AutonomousWorker
from forgefleet.engine.node_manager import NodeManager
from forgefleet.engine.peer_mesh import PeerMesh

# Global mesh for status endpoint to access
_mesh = None

def start_status_server(node_mgr, mesh, port=51820):
    """HTTP server — other nodes query this one."""
    from http.server import HTTPServer, BaseHTTPRequestHandler
    import json
    
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path == "/api/status" or self.path == "/health":
                status = node_mgr.get_status()
                status["current_task"] = mesh.current_task if mesh else ""
                status["coordinator"] = mesh.coordinator if mesh else ""
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps(status).encode())
            elif self.path == "/api/fleet":
                fleet_view = mesh.get_fleet_view() if mesh else {}
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps(fleet_view).encode())
            else:
                self.send_response(404)
                self.end_headers()
        def log_message(self, *args): pass
    
    try:
        server = HTTPServer(("0.0.0.0", port), Handler)
        server.serve_forever()
    except:
        pass

if __name__ == "__main__":
    repo = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/taylorProjects/HireFlow360")
    
    # Start node manager (self-heal every 60s)
    node_mgr = NodeManager()
    monitor_thread = threading.Thread(target=node_mgr.run_monitor, args=(60,), daemon=True)
    monitor_thread.start()
    
    # Start peer mesh (discover peers, elect coordinator, share state)
    mesh = PeerMesh()
    _mesh = mesh
    mesh_thread = threading.Thread(target=mesh.run_mesh, args=(30,), daemon=True)
    mesh_thread.start()
    
    # Start HTTP status + fleet view endpoint
    status_thread = threading.Thread(target=start_status_server, args=(node_mgr, mesh, 51820), daemon=True)
    status_thread.start()
    
    print(f"[{node_mgr.node_name}] Agent + NodeManager + PeerMesh + Status:51820 started", flush=True)
    
    # Run the autonomous worker (builds tickets)
    worker = AutonomousWorker(repo_dir=repo)
    worker.run()
