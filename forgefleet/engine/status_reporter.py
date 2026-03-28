"""Status Reporter — ForgeFleet sends its own status to Telegram.

No more Taylor generating fleet reports. ForgeFleet reports itself.
Runs on a schedule (hourly) or on-demand via MCP.
"""
import json
import os
import subprocess
import time
from datetime import datetime, date
from .fleet_router import FleetRouter
from .mc_client import MCClient
from .docker_monitor import DockerMonitor
from .openclaw_bridge import OpenClawBridge


class StatusReporter:
    """ForgeFleet generates and sends its own status reports."""
    
    def __init__(self):
        self.notify = OpenClawBridge()
    
    def generate_report(self) -> str:
        """Generate a full status report from ForgeFleet's own data."""
        lines = [f"🤖 ForgeFleet Status — {datetime.now().strftime('%a %b %d, %I:%M %p')}"]
        
        # LLM Endpoints
        try:
            router = FleetRouter()
            healthy = [ep for ep in router.endpoints if ep.healthy]
            busy = [ep for ep in router.endpoints if ep.busy]
            lines.append(f"\n⚡ LLMs: {len(healthy)}/{len(router.endpoints)} healthy, {len(busy)} busy")
            
            # Map IPs to node names
            from forgefleet import config as ff_config
            ip_to_name = {node.get("ip", ""): name for name, node in ff_config.get_nodes().items()}
            
            by_tier = {}
            for ep in router.endpoints:
                by_tier.setdefault(ep.tier, []).append(ep)
            
            tier_names = {1: "9B fast", 2: "32B code", 3: "72B review", 4: "235B expert"}
            for tier in sorted(by_tier.keys()):
                eps = by_tier[tier]
                names = ", ".join(ip_to_name.get(ep.ip, ep.ip) for ep in eps)
                busy_count = sum(1 for ep in eps if ep.busy)
                status = f"({busy_count} busy)" if busy_count else "(idle)"
                lines.append(f"  T{tier} {tier_names.get(tier, '')}: {len(eps)} {status} — {names}")
        except Exception as e:
            lines.append(f"\n⚡ LLMs: error — {e}")
        
        # Sub-agents
        try:
            from forgefleet import config
            agent_status = []
            for name in config.get_nodes():
                ip = config.get_node_ip(name)
                try:
                    r = subprocess.run(
                        ["ssh", "-o", "ConnectTimeout=3", "-o", "BatchMode=yes", ip,
                         "systemctl --user is-active forgefleet-agent 2>/dev/null || "
                         "ps aux | grep forgefleet_subagent | grep -v grep | wc -l"],
                        capture_output=True, text=True, timeout=5,
                    )
                    state = r.stdout.strip()
                    agent_status.append(f"{name}: {state}")
                except:
                    agent_status.append(f"{name}: ?")
            
            lines.append(f"\n🤖 Agents: {', '.join(agent_status)}")
        except Exception as e:
            lines.append(f"\n🤖 Agents: error — {e}")
        
        # Tickets
        try:
            mc = MCClient()
            if mc.health():
                stats = mc.stats()
                lines.append(
                    f"\n📊 Tickets: {stats.get('done', 0)} done, "
                    f"{stats.get('claimable', 0)} todo, "
                    f"{stats.get('review', 0)} review, "
                    f"{stats.get('blocked', 0)} blocked"
                )
        except:
            pass
        
        # Docker
        try:
            docker = DockerMonitor()
            containers = docker.check_all()
            running = sum(1 for c in containers if c.running)
            down = [c.name for c in containers if not c.running]
            lines.append(f"\n🐳 Docker: {running}/{len(containers)} running")
            if down:
                lines.append(f"  ❌ Down: {', '.join(down)}")
        except:
            pass
        
        # Build stats today
        try:
            log_path = os.path.expanduser(f"~/.forgefleet/logs/builds-{date.today()}.jsonl")
            if os.path.exists(log_path):
                log_lines = open(log_path).readlines()
                completed = sum(1 for l in log_lines if '"completed"' in l)
                failed = sum(1 for l in log_lines if '"failed"' in l)
                lines.append(f"\n🔨 Builds today: {completed}✅ {failed}❌")
            else:
                lines.append(f"\n🔨 No builds logged today")
        except:
            pass
        
        # Git activity
        try:
            r = subprocess.run(
                ["git", "log", "--all", "--oneline", "--since=24 hours ago"],
                capture_output=True, text=True, timeout=10,
                cwd=os.path.expanduser("~/taylorProjects/HireFlow360"),
            )
            commit_count = len(r.stdout.strip().split("\n")) if r.stdout.strip() else 0
            lines.append(f"\n🔀 Git: {commit_count} commits in last 24h")
        except:
            pass
        
        # Code sync — are all nodes on latest?
        try:
            local_hash = subprocess.run(
                ["git", "rev-parse", "HEAD"], capture_output=True, text=True,
                timeout=5, cwd=os.path.expanduser("~/taylorProjects/forge-fleet")
            ).stdout.strip()[:8]
            
            from forgefleet import config
            sync_status = []
            for name in config.get_nodes():
                ip = config.get_node_ip(name)
                try:
                    r = subprocess.run(
                        ["ssh", "-o", "ConnectTimeout=3", ip,
                         "ls ~/taylorProjects/forge-fleet/forgefleet/engine/docker_monitor.py 2>/dev/null && echo 'latest' || echo 'outdated'"],
                        capture_output=True, text=True, timeout=5,
                    )
                    sync_status.append(f"{name}:{'✅' if 'latest' in r.stdout else '❌'}")
                except:
                    sync_status.append(f"{name}:?")
            
            lines.append(f"\n📦 Code sync: {' '.join(sync_status)}")
        except:
            pass
        
        # Active tasks — what's being built on each node
        try:
            active = []
            from forgefleet import config
            for name in config.get_nodes():
                ip = config.get_node_ip(name)
                try:
                    r = subprocess.run(
                        ["ssh", "-o", "ConnectTimeout=3", ip,
                         "tail -1 ~/forgefleet-agent.log 2>/dev/null | strings | head -c 50"],
                        capture_output=True, text=True, timeout=5,
                    )
                    last_line = r.stdout.strip()
                    if last_line and "Iteration" in last_line:
                        active.append(f"{name}: building")
                    elif last_line:
                        active.append(f"{name}: {last_line[:30]}")
                except:
                    pass
            
            if active:
                lines.append(f"\n🏗️ Active: {', '.join(active)}")
        except:
            pass
        
        # Evolution stats
        try:
            from .evolution import EvolutionEngine
            evo = EvolutionEngine()
            rate = evo._overall_success_rate()
            lines.append(f"\n📈 Success rate: {rate}")
            evo.close()
        except:
            pass
        
        return "\n".join(lines)
    
    def send_report(self):
        """Generate and send status report to Telegram."""
        report = self.generate_report()
        self.notify.send_message(report)
        return report
    
    def should_send(self, interval_seconds: int = 3600) -> bool:
        """Check if enough time has passed since last report."""
        state_file = os.path.expanduser("~/.forgefleet/last_status_report.txt")
        
        if os.path.exists(state_file):
            try:
                last = float(open(state_file).read().strip())
                if time.time() - last < interval_seconds:
                    return False
            except:
                pass
        
        # Update timestamp
        os.makedirs(os.path.dirname(state_file), exist_ok=True)
        open(state_file, "w").write(str(time.time()))
        return True
