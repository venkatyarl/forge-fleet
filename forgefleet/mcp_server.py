"""ForgeFleet MCP Server — exposes fleet orchestration as MCP tools.

Runs as a stdio MCP server that any MCP client can connect to:
- Claude Desktop (via claude_desktop_config.json)
- OpenClaw (via openclaw mcp set)
- Cursor, VS Code Copilot, Gemini CLI, etc.

Tools exposed:
- fleet_status: health + busy state of all nodes and models
- fleet_scan: discover new LLM endpoints on the network
- fleet_run: execute a task through the tiered agent pipeline
- fleet_ssh: run a command on a remote node
- fleet_install_model: download and start a model on a node
- fleet_crew: run a multi-agent crew on a coding task
"""
import json
import sys
import os

# Add parent to path for imports
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from forgefleet.engine.discovery import NetworkDiscovery, TIER_NAMES
from forgefleet.engine.fleet_router import FleetRouter
from forgefleet.engine.llm import LLM
from forgefleet.engine.agent import Agent
from forgefleet.engine.task import Task
from forgefleet.engine.crew import Crew
from forgefleet.engine.tool import Tool
import subprocess
import time


# ─── MCP Protocol Implementation (stdio JSON-RPC) ──────

class MCPServer:
    """Minimal MCP server over stdio. No dependencies.
    
    Implements just enough of the MCP protocol:
    - initialize handshake
    - tools/list
    - tools/call
    """
    
    def __init__(self):
        self.discovery = NetworkDiscovery()
        self.router = FleetRouter()
        self.tools = self._register_tools()
    
    def _register_tools(self) -> dict:
        """Register all ForgeFleet tools."""
        return {
            "fleet_status": {
                "description": "Get health and busy status of all fleet nodes and models. Shows which LLMs are running, their tier, context size, and current load.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "refresh": {
                            "type": "boolean",
                            "description": "Force re-scan of all endpoints (default: use cached)",
                            "default": False,
                        }
                    },
                },
            },
            "fleet_scan": {
                "description": "Scan the local network for new LLM endpoints. Discovers llama.cpp, Ollama, and vLLM servers automatically on ports 51800-51803.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["known", "full"],
                            "description": "known=scan known IPs only (fast), full=scan entire /24 subnet",
                            "default": "known",
                        }
                    },
                },
            },
            "fleet_run": {
                "description": "Execute a prompt through the tiered LLM pipeline. Starts at the fastest model, escalates to more powerful ones if needed. Returns the result from whichever tier succeeds.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "The prompt/task to execute",
                        },
                        "start_tier": {
                            "type": "integer",
                            "description": "Start at tier (1=9B fast, 2=32B code, 3=72B review, 4=235B expert)",
                            "default": 1,
                        },
                        "max_tier": {
                            "type": "integer",
                            "description": "Maximum tier to escalate to",
                            "default": 4,
                        },
                    },
                    "required": ["prompt"],
                },
            },
            "fleet_ssh": {
                "description": "Run a command on a remote fleet node via SSH. Use node name (taylor, james, marcus, sophie, priya, ace) or IP address.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "node": {
                            "type": "string",
                            "description": "Node name or IP address",
                        },
                        "command": {
                            "type": "string",
                            "description": "Shell command to execute",
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Timeout in seconds",
                            "default": 30,
                        },
                    },
                    "required": ["node", "command"],
                },
            },
            "fleet_install_model": {
                "description": "Download and start an LLM model on a fleet node. Downloads the GGUF file via URL, starts llama-server, and verifies the endpoint.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "node": {
                            "type": "string",
                            "description": "Node name or IP to install on",
                        },
                        "model_url": {
                            "type": "string",
                            "description": "URL to download the GGUF model file",
                        },
                        "model_path": {
                            "type": "string",
                            "description": "Where to save the model on the node (e.g., ~/models/qwen-9b.gguf)",
                        },
                        "port": {
                            "type": "integer",
                            "description": "Port to start llama-server on",
                            "default": 51802,
                        },
                        "ctx_size": {
                            "type": "integer",
                            "description": "Context window size",
                            "default": 8192,
                        },
                    },
                    "required": ["node", "model_url", "model_path"],
                },
            },
            "fleet_wait": {
                "description": "Wait until a fleet condition is met (e.g., model finishes loading, node comes online). Polls every 10 seconds up to the timeout. Returns immediately if condition already met.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "condition": {
                            "type": "string",
                            "enum": ["all_healthy", "tier_available", "model_loaded"],
                            "description": "What to wait for: all_healthy=all known endpoints up, tier_available=specific tier has an available model, model_loaded=specific IP:port is healthy",
                        },
                        "tier": {
                            "type": "integer",
                            "description": "For tier_available: which tier to wait for (1-4)",
                        },
                        "endpoint": {
                            "type": "string",
                            "description": "For model_loaded: IP:port to wait for (e.g., 192.168.5.100:51800)",
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Max seconds to wait (default 300 = 5 min)",
                            "default": 300,
                        },
                    },
                    "required": ["condition"],
                },
            },
            "fleet_config": {
                "description": "Get or set ForgeFleet configuration. Single source of truth for all fleet data — nodes, services, notifications, ports. Other services (OpenClaw, MC, Claude) should query this instead of reading fleet.json directly.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["get_all", "get_nodes", "get_node", "get_services", "set"],
                            "description": "What to do",
                            "default": "get_all",
                        },
                        "key": {
                            "type": "string",
                            "description": "Config key (for get/set)",
                        },
                        "value": {
                            "type": "string",
                            "description": "Value to set (for set action)",
                        },
                    },
                },
            },
            "fleet_crew": {
                "description": "Run a multi-agent coding crew on a task. Three agents work sequentially: Context Engineer (9B) researches the codebase, Code Writer (32B) implements, Code Reviewer (72B) verifies.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "Description of the coding task",
                        },
                        "repo_dir": {
                            "type": "string",
                            "description": "Path to the repository",
                            "default": ".",
                        },
                    },
                    "required": ["task"],
                },
            },
        }
    
    def _resolve_node(self, node: str) -> str:
        """Resolve node name to IP address."""
        node_map = {
            "taylor": "192.168.5.100",
            "james": "192.168.5.108",
            "marcus": "192.168.5.102",
            "sophie": "192.168.5.103",
            "priya": "192.168.5.106",
            "ace": "192.168.5.104",
        }
        return node_map.get(node.lower(), node)
    
    def handle_tool(self, name: str, args: dict) -> str:
        """Execute a tool and return the result."""
        
        if name == "fleet_status":
            if args.get("refresh"):
                endpoints = self.discovery.scan_known_hosts()
            else:
                if not self.discovery.discovered:
                    self.discovery.scan_known_hosts()
                endpoints = self.discovery.discovered
            
            lines = [f"Fleet Status — {len(endpoints)} endpoints found\n"]
            by_tier = {}
            for ep in endpoints:
                by_tier.setdefault(ep.tier, []).append(ep)
            
            for tier in sorted(by_tier.keys()):
                tier_name = TIER_NAMES.get(tier, "unknown")
                lines.append(f"\n## Tier {tier} — {tier_name}")
                for ep in by_tier[tier]:
                    busy = "🔴 BUSY" if ep.slots_busy else "🟢 idle"
                    lines.append(
                        f"  {ep.hostname}/{ep.model_name} @ {ep.url} "
                        f"[ctx:{ep.ctx_size}, timeout:{ep.timeout}s] {busy}"
                    )
            
            return "\n".join(lines)
        
        elif name == "fleet_scan":
            mode = args.get("mode", "known")
            if mode == "full":
                endpoints = self.discovery.scan_subnet()
            else:
                endpoints = self.discovery.scan_known_hosts()
            
            lines = [f"Scan complete: {len(endpoints)} LLM endpoints discovered\n"]
            for ep in sorted(endpoints, key=lambda e: (e.tier, e.ip)):
                tier_name = TIER_NAMES.get(ep.tier, "?")
                lines.append(
                    f"  T{ep.tier} {ep.hostname}/{ep.model_name} "
                    f"@ {ep.url} [ctx:{ep.ctx_size}]"
                )
            return "\n".join(lines)
        
        elif name == "fleet_run":
            prompt = args["prompt"]
            start_tier = args.get("start_tier", 1)
            max_tier = args.get("max_tier", 4)
            
            # Use cached router if available, refresh discovery only if empty
            if not self.router.endpoints:
                self.router = FleetRouter()
            result = self.router.tiered_execute(prompt, start_tier, max_tier)
            
            return (
                f"Tier: {result['tier']} ({TIER_NAMES.get(result['tier'], '?')})\n"
                f"Model: {result['model']}\n"
                f"Success: {result['success']}\n\n"
                f"{result['result']}"
            )
        
        elif name == "fleet_ssh":
            ip = self._resolve_node(args["node"])
            command = args["command"]
            timeout = args.get("timeout", 30)
            
            try:
                r = subprocess.run(
                    ["ssh", "-o", "ConnectTimeout=5", "-o", "StrictHostKeyChecking=no",
                     ip, command],
                    capture_output=True, text=True, timeout=timeout
                )
                output = r.stdout + r.stderr
                if len(output) > 8000:
                    output = output[:4000] + "\n...[truncated]...\n" + output[-4000:]
                return f"Exit code: {r.returncode}\n{output}"
            except subprocess.TimeoutExpired:
                return f"Command timed out after {timeout}s"
            except Exception as e:
                return f"SSH error: {e}"
        
        elif name == "fleet_wait":
            condition = args["condition"]
            timeout = args.get("timeout", 300)
            start_time = time.time()
            poll_interval = 10
            
            while time.time() - start_time < timeout:
                if condition == "all_healthy":
                    endpoints = self.discovery.scan_known_hosts()
                    all_ok = all(
                        ep.model_name != "loading..." and ep.healthy
                        for ep in endpoints if ep.tier > 0
                    )
                    if all_ok and endpoints:
                        return f"✅ All {len(endpoints)} endpoints are healthy"
                
                elif condition == "tier_available":
                    tier = args.get("tier", 4)
                    self.router = FleetRouter()
                    available = self.router.get_available(tier)
                    if available:
                        ep = available[0]
                        return f"✅ Tier {tier} available: {ep.name} @ http://{ep.ip}:{ep.port}"
                
                elif condition == "model_loaded":
                    endpoint_str = args.get("endpoint", "192.168.5.100:51800")
                    ip, port = endpoint_str.split(":")
                    try:
                        import urllib.request as ur
                        req = ur.Request(f"http://{ip}:{port}/health")
                        with ur.urlopen(req, timeout=5) as resp:
                            data = json.loads(resp.read())
                            if data.get("status") == "ok":
                                return f"✅ Model at {endpoint_str} is loaded and healthy"
                    except Exception:
                        pass
                
                elapsed = int(time.time() - start_time)
                remaining = timeout - elapsed
                if remaining <= 0:
                    break
                time.sleep(min(poll_interval, remaining))
            
            elapsed = int(time.time() - start_time)
            return f"⏰ Timeout after {elapsed}s waiting for {condition}"
        
        elif name == "fleet_config":
            from forgefleet import config as ff_config
            action = args.get("action", "get_all")
            
            if action == "get_all":
                return json.dumps(ff_config.get_all(), indent=2)
            elif action == "get_nodes":
                return json.dumps(ff_config.get_nodes(), indent=2)
            elif action == "get_node":
                node_name = args.get("key", "")
                return json.dumps(ff_config.get_node(node_name), indent=2)
            elif action == "get_services":
                return json.dumps(ff_config.get("services", {}), indent=2)
            elif action == "set":
                key = args.get("key", "")
                value = args.get("value", "")
                if key:
                    ff_config.set_value(key, value)
                    return f"Set {key} = {value}"
                return "No key provided"
            return "Unknown action"
        
        elif name == "fleet_install_model":
            ip = self._resolve_node(args["node"])
            result = self.discovery.install_model(
                ip=ip,
                model_url=args["model_url"],
                model_path=args["model_path"],
                port=args.get("port", 51802),
                ctx_size=args.get("ctx_size", 8192),
            )
            
            lines = [f"Install on {args['node']} ({ip}):"]
            for step in result["steps"]:
                lines.append(f"  → {step}")
            lines.append(f"\nSuccess: {result['success']}")
            if result.get("endpoint"):
                ep = result["endpoint"]
                lines.append(f"Endpoint: {ep['model']} (T{ep['tier']}) @ {ep['url']}")
            return "\n".join(lines)
        
        elif name == "fleet_crew":
            task_desc = args["task"]
            repo_dir = args.get("repo_dir", "/Users/venkat/taylorProjects/HireFlow360")
            
            # Build file tools scoped to the repo
            def read_file(filepath=""):
                full = os.path.join(repo_dir, filepath)
                if not os.path.exists(full):
                    return f"Not found: {filepath}"
                content = open(full).read()
                return content[:6000] if len(content) > 6000 else content
            
            def write_file(filepath="", content=""):
                full = os.path.join(repo_dir, filepath)
                os.makedirs(os.path.dirname(full), exist_ok=True)
                open(full, "w").write(content)
                return f"Written: {filepath} ({len(content)} chars)"
            
            def list_files(directory=".", pattern=""):
                full = os.path.join(repo_dir, directory)
                exclude = {"target", "node_modules", ".git", "dist", ".next", "__pycache__"}
                files = []
                for root, dirs, fnames in os.walk(full):
                    dirs[:] = [d for d in dirs if d not in exclude]
                    for f in fnames:
                        if pattern and not f.endswith(pattern):
                            continue
                        files.append(os.path.relpath(os.path.join(root, f), repo_dir))
                    if len(files) > 100:
                        break
                return "\n".join(files[:100])
            
            def run_command(command=""):
                try:
                    r = subprocess.run(command, shell=True, capture_output=True,
                                       text=True, timeout=60, cwd=repo_dir)
                    out = r.stdout + r.stderr
                    return out[:4000] if len(out) > 4000 else out
                except Exception as e:
                    return f"Error: {e}"
            
            # Create tools
            tools = [
                Tool(name="read_file", description="Read a file",
                     parameters={"type": "object", "properties": {"filepath": {"type": "string"}}},
                     func=read_file),
                Tool(name="write_file", description="Write a file",
                     parameters={"type": "object", "properties": {"filepath": {"type": "string"}, "content": {"type": "string"}}},
                     func=write_file),
                Tool(name="list_files", description="List files in directory",
                     parameters={"type": "object", "properties": {"directory": {"type": "string"}, "pattern": {"type": "string"}}},
                     func=list_files),
                Tool(name="run_command", description="Run a shell command",
                     parameters={"type": "object", "properties": {"command": {"type": "string"}}},
                     func=run_command),
            ]
            
            # Get fleet-aware LLMs (use cached router)
            if not self.router.endpoints:
                self.router = FleetRouter()
            llm_fast = self.router.get_llm(1) or LLM(base_url="http://192.168.5.100:51803/v1")
            llm_code = self.router.get_llm(2) or llm_fast
            llm_review = self.router.get_llm(3) or llm_code
            
            # Build the crew
            researcher = Agent(
                role="Context Engineer", goal="Find relevant code for the task",
                backstory="You scan codebases quickly to find what's relevant.",
                tools=[tools[0], tools[2], tools[3]], llm=llm_fast,
                verbose=False, max_iterations=8,
            )
            
            coder = Agent(
                role="Senior Developer", goal="Write production-quality code",
                backstory="You write complete, working code. Never placeholders or TODOs.",
                tools=tools, llm=llm_code,
                verbose=False, max_iterations=10,
            )
            
            reviewer = Agent(
                role="Code Reviewer", goal="Verify the code compiles and is correct",
                backstory="You catch bugs, missing error handling, and placeholder code.",
                tools=[tools[0], tools[2], tools[3]], llm=llm_review,
                verbose=False, max_iterations=6,
            )
            
            t1 = Task(description=f"Research the codebase for: {task_desc}", agent=researcher)
            t2 = Task(description=f"Implement: {task_desc}", agent=coder, context_tasks=[t1])
            t3 = Task(description=f"Review the implementation of: {task_desc}", agent=reviewer, context_tasks=[t1, t2])
            
            crew = Crew(agents=[researcher, coder, reviewer], tasks=[t1, t2, t3], verbose=False)
            result = crew.kickoff()
            
            lines = [f"Crew completed in {result['total_time']}s"]
            for r in result["results"]:
                icon = "✅" if r["success"] else "❌"
                lines.append(f"\n{icon} {r['agent']} ({r['time']}s):")
                lines.append(r["output"][:2000])
            
            return "\n".join(lines)
        
        return f"Unknown tool: {name}"
    
    def run(self):
        """Run the MCP server on stdio."""
        while True:
            try:
                line = sys.stdin.readline()
                if not line:
                    break
                
                request = json.loads(line.strip())
                method = request.get("method", "")
                req_id = request.get("id")
                params = request.get("params", {})
                
                if method == "initialize":
                    response = {
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {"tools": {}},
                            "serverInfo": {
                                "name": "forgefleet",
                                "version": "0.2.0",
                            },
                        },
                    }
                
                elif method == "notifications/initialized":
                    continue  # No response needed for notifications
                
                elif method == "tools/list":
                    tool_list = []
                    for name, info in self.tools.items():
                        tool_list.append({
                            "name": name,
                            "description": info["description"],
                            "inputSchema": info["inputSchema"],
                        })
                    response = {
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {"tools": tool_list},
                    }
                
                elif method == "tools/call":
                    tool_name = params.get("name", "")
                    tool_args = params.get("arguments", {})
                    
                    try:
                        result = self.handle_tool(tool_name, tool_args)
                    except Exception as e:
                        result = f"Error: {e}"
                    
                    response = {
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {
                            "content": [{"type": "text", "text": result}],
                        },
                    }
                
                else:
                    response = {
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": {"code": -32601, "message": f"Unknown method: {method}"},
                    }
                
                sys.stdout.write(json.dumps(response) + "\n")
                sys.stdout.flush()
                
            except json.JSONDecodeError:
                continue
            except Exception as e:
                if req_id:
                    error_resp = {
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": {"code": -32603, "message": str(e)},
                    }
                    sys.stdout.write(json.dumps(error_resp) + "\n")
                    sys.stdout.flush()


if __name__ == "__main__":
    server = MCPServer()
    server.run()
