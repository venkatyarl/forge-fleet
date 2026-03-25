"""Web Dashboard — simple HTML page for fleet status.

Item #7: One-page dashboard showing fleet status, running tasks, event log.
Serves on port 51820. No external dependencies — built-in http.server.
"""
import json
import os
import time
from http.server import HTTPServer, BaseHTTPRequestHandler
from dataclasses import dataclass
import threading


DASHBOARD_HTML = """<!DOCTYPE html>
<html lang="en" data-theme="dark">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>ForgeFleet Dashboard</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { font-family: -apple-system, system-ui, sans-serif; background: #0a0a0a; color: #e0e0e0; padding: 20px; }
  h1 { color: #00b4d8; margin-bottom: 20px; }
  h2 { color: #7c3aed; margin: 20px 0 10px; font-size: 1.2em; }
  .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 16px; }
  .card { background: #1a1a2e; border: 1px solid #333; border-radius: 12px; padding: 16px; }
  .card.healthy { border-color: #10b981; }
  .card.loading { border-color: #f59e0b; }
  .card.down { border-color: #ef4444; }
  .tier { font-size: 0.8em; color: #888; text-transform: uppercase; }
  .model { font-weight: bold; font-size: 1.1em; }
  .node { color: #00b4d8; }
  .status { display: inline-block; padding: 2px 8px; border-radius: 4px; font-size: 0.85em; }
  .status.ok { background: #10b98133; color: #10b981; }
  .status.busy { background: #f59e0b33; color: #f59e0b; }
  .status.down { background: #ef444433; color: #ef4444; }
  .stats { display: flex; gap: 20px; margin: 10px 0; flex-wrap: wrap; }
  .stat { text-align: center; }
  .stat-value { font-size: 2em; font-weight: bold; color: #00b4d8; }
  .stat-label { font-size: 0.8em; color: #888; }
  .events { max-height: 300px; overflow-y: auto; font-family: monospace; font-size: 0.85em; }
  .event { padding: 4px 0; border-bottom: 1px solid #222; }
  .refresh { color: #888; font-size: 0.8em; }
  a { color: #00b4d8; }
</style>
</head>
<body>
<h1>⚡ ForgeFleet Dashboard</h1>
<p class="refresh">Auto-refreshes every 10s | <a href="/api/status">API</a></p>

<div class="stats" id="stats"></div>

<h2>Fleet Endpoints</h2>
<div class="grid" id="endpoints"></div>

<h2>Recent Events</h2>
<div class="events" id="events"></div>

<script>
async function refresh() {
  try {
    const res = await fetch('/api/status');
    const data = await res.json();
    
    // Stats
    document.getElementById('stats').innerHTML = `
      <div class="stat"><div class="stat-value">${data.endpoints?.length || 0}</div><div class="stat-label">Endpoints</div></div>
      <div class="stat"><div class="stat-value">${data.healthy || 0}</div><div class="stat-label">Healthy</div></div>
      <div class="stat"><div class="stat-value">${data.busy || 0}</div><div class="stat-label">Busy</div></div>
      <div class="stat"><div class="stat-value">${data.tickets?.total || 0}</div><div class="stat-label">Tickets</div></div>
      <div class="stat"><div class="stat-value">${data.tickets?.done || 0}</div><div class="stat-label">Done</div></div>
    `;
    
    // Endpoints
    const eps = data.endpoints || [];
    document.getElementById('endpoints').innerHTML = eps.map(ep => {
      const cls = ep.healthy ? (ep.busy ? 'loading' : 'healthy') : 'down';
      const statusCls = ep.healthy ? (ep.busy ? 'busy' : 'ok') : 'down';
      const statusText = ep.healthy ? (ep.busy ? 'BUSY' : 'OK') : 'DOWN';
      return `<div class="card ${cls}">
        <div class="tier">Tier ${ep.tier}</div>
        <div class="model">${ep.model}</div>
        <div class="node">${ep.node} (${ep.ip}:${ep.port})</div>
        <span class="status ${statusCls}">${statusText}</span>
        <span style="color:#888;font-size:0.8em">ctx:${ep.ctx_size || '?'}</span>
      </div>`;
    }).join('');
    
    // Events  
    const events = data.events || [];
    document.getElementById('events').innerHTML = events.map(e => 
      `<div class="event">${e.icon || '📋'} ${e.message}</div>`
    ).join('') || '<div class="event">No recent events</div>';
    
  } catch(e) { console.error(e); }
}
refresh();
setInterval(refresh, 10000);
</script>
</body>
</html>"""


class DashboardHandler(BaseHTTPRequestHandler):
    """HTTP handler for the dashboard."""
    
    status_func = None  # Set by DashboardServer
    
    def do_GET(self):
        if self.path == "/" or self.path == "/dashboard":
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.end_headers()
            self.wfile.write(DASHBOARD_HTML.encode())
        elif self.path == "/api/status":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            status = self.status_func() if self.status_func else {}
            self.wfile.write(json.dumps(status).encode())
        else:
            self.send_response(404)
            self.end_headers()
    
    def log_message(self, format, *args):
        pass  # Suppress access logs


@dataclass
class DashboardServer:
    """Serve the ForgeFleet dashboard on port 51820."""
    port: int = 51820
    status_func: callable = None
    _server: HTTPServer = None
    _thread: threading.Thread = None
    
    def start(self):
        """Start the dashboard server in background."""
        DashboardHandler.status_func = self.status_func
        self._server = HTTPServer(("0.0.0.0", self.port), DashboardHandler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()
        print(f"📊 Dashboard: http://localhost:{self.port}")
    
    def stop(self):
        if self._server:
            self._server.shutdown()
