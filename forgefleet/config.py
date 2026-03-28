"""ForgeFleet Configuration — SINGLE SOURCE OF TRUTH.

One config file: ~/.forgefleet/config.json
Contains EVERYTHING: nodes, services, notifications, ports.
Exposed via MCP server — OpenClaw, MC, Claude all read from here.

No more fleet.json, no more scattered configs.
"""
import json
import os
import socket
from dataclasses import dataclass, field


CONFIG_PATH = os.path.expanduser("~/.forgefleet/config.json")


def _load() -> dict:
    """Load the master config."""
    if os.path.exists(CONFIG_PATH):
        try:
            with open(CONFIG_PATH) as f:
                return json.load(f)
        except:
            pass
    return {}


def _save(config: dict):
    """Save the master config."""
    os.makedirs(os.path.dirname(CONFIG_PATH), exist_ok=True)
    with open(CONFIG_PATH, "w") as f:
        json.dump(config, f, indent=2)


def get(key: str, default=None):
    """Get a config value. env var → config file → default."""
    env_key = f"FORGEFLEET_{key.upper()}"
    env_val = os.environ.get(env_key)
    if env_val is not None:
        return env_val
    config = _load()
    return config.get(key, default)


def set_value(key: str, value):
    """Set a config value."""
    config = _load()
    config[key] = value
    _save(config)


def get_all() -> dict:
    """Get the entire config."""
    return _load()


# ─── Convenience accessors ──────────────────────────────

def get_nodes() -> dict:
    """Get all fleet nodes."""
    return get("nodes", {})


def get_node(name: str) -> dict:
    """Get a specific node's config."""
    return get_nodes().get(name, {})


def get_node_ip(name: str) -> str:
    """Get a node's primary IP."""
    return get_node(name).get("ip", "")


def get_gateway_node() -> str:
    """Find which node is the gateway."""
    for name, node in get_nodes().items():
        if node.get("role") == "gateway":
            return name
    return ""


def get_mc_url() -> str:
    """Get Mission Control URL — derived from gateway node."""
    mc_port = get("services", {}).get("mc", {}).get("port", 60002)
    gateway = get_gateway_node()
    if gateway:
        ip = get_node_ip(gateway)
        return f"http://{ip}:{mc_port}"
    return get("mc_url", "http://localhost:60002")


def get_telegram_config() -> dict:
    """Get Telegram notification config."""
    return get("notifications", {}).get("telegram", {})


def get_llm_ports() -> list:
    """Get LLM scan ports."""
    return get("llm_ports", [51800, 51801, 51802, 51803])


def get_local_ip() -> str:
    """Get this machine's LAN IP."""
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except:
        return "127.0.0.1"


def get_node_name() -> str:
    """Get this machine's node name from config."""
    local_ip = get_local_ip()
    for name, node in get_nodes().items():
        if node.get("ip") == local_ip:
            return name
        if local_ip in node.get("alt_ips", []):
            return name
    return os.uname().nodename.split(".")[0].lower()


# ─── Initialize default config if missing ───────────────

def init_default():
    """Create default config if none exists. Merges fleet.json if available."""
    if os.path.exists(CONFIG_PATH):
        config = _load()
        if config.get("nodes"):
            return  # Already has nodes — don't overwrite
    
    config = _load() or {}
    
    # Try to import from fleet.json
    for fleet_path in [
        os.path.expanduser("~/fleet.json"),
        os.path.expanduser("~/.openclaw/workspace/fleet.json"),
    ]:
        if os.path.exists(fleet_path):
            try:
                with open(fleet_path) as f:
                    fleet = json.load(f)
                config["nodes"] = fleet.get("nodes", {})
                break
            except:
                pass
    
    # Set defaults for anything missing
    config.setdefault("nodes", {})
    config.setdefault("services", {
        "mc": {"port": 60002},
        "forgefleet": {"port": 51820},
        "hireflow_backend": {"port": 8180},
        "hireflow_frontend": {"port": 3100},
    })
    config.setdefault("notifications", {
        "telegram": {
            "chat_id": "8496613333",
            "channel": "telegram",
        }
    })
    config.setdefault("llm_ports", [51800, 51801, 51802, 51803])
    config.setdefault("announce_port", 50099)
    config.setdefault("tier_timeouts", {
        "1": 120, "2": 300, "3": 600, "4": 900,
    })
    
    _save(config)


# Auto-initialize on import
init_default()
