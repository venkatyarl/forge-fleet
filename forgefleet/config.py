"""ForgeFleet Configuration — reads fleet.toml, hot-reloads on change.

Single file: ~/.forgefleet/fleet.toml
Human-friendly TOML format with comments.
Auto-reloads when file changes — no restart needed.
"""
import os
import socket
import time
import tomllib  # Built into Python 3.11+


CONFIG_PATH = os.path.expanduser("~/.forgefleet/fleet.toml")
_cache = {}
_cache_mtime = 0


def _load() -> dict:
    """Load fleet.toml with caching. Reloads if file changed."""
    global _cache, _cache_mtime
    
    if not os.path.exists(CONFIG_PATH):
        return _cache or {}
    
    mtime = os.path.getmtime(CONFIG_PATH)
    if mtime != _cache_mtime:
        try:
            with open(CONFIG_PATH, "rb") as f:
                _cache = tomllib.load(f)
            _cache_mtime = mtime
        except Exception as e:
            print(f"Config load error: {e}")
    
    return _cache


def get(key: str, default=None):
    """Get a config value. Supports dot notation: 'notifications.telegram.chat_id'"""
    # Check env var first
    env_key = f"FORGEFLEET_{key.upper().replace('.', '_')}"
    env_val = os.environ.get(env_key)
    if env_val is not None:
        return env_val
    
    # Walk the config tree
    config = _load()
    parts = key.split(".")
    current = config
    for part in parts:
        if isinstance(current, dict) and part in current:
            current = current[part]
        else:
            return default
    return current


def get_all() -> dict:
    """Get the entire config (auto-reloads if changed)."""
    return _load()


# ─── Convenience accessors ──────────────────────────────

def get_nodes() -> dict:
    return get("nodes", {})

def get_node(name: str) -> dict:
    return get_nodes().get(name, {})

def get_node_ip(name: str) -> str:
    return get_node(name).get("ip", "")

def get_gateway_node() -> str:
    for name, node in get_nodes().items():
        if node.get("role") == "gateway":
            return name
    return ""

def get_mc_url() -> str:
    mc_port = get("services.mc.port", 60002)
    gateway = get_gateway_node()
    if gateway:
        ip = get_node_ip(gateway)
        return f"http://{ip}:{mc_port}"
    return "http://localhost:60002"

def get_telegram_config() -> dict:
    return get("notifications.telegram", {})

def get_llm_ports() -> list:
    return get("llm.ports", [51800, 51801, 51802, 51803])

def get_tier_timeout(tier: int) -> int:
    return get(f"llm.timeouts.tier{tier}", 300)

def get_local_ip() -> str:
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except:
        return "127.0.0.1"

def get_node_name() -> str:
    local_ip = get_local_ip()
    for name, node in get_nodes().items():
        if node.get("ip") == local_ip:
            return name
        if local_ip in node.get("alt_ips", []):
            return name
    return os.uname().nodename.split(".")[0].lower()

def set_value(key: str, value):
    """Set a value — appends to TOML file (simple key=value at end)."""
    # For complex updates, edit the file directly
    # This is a simple append for runtime overrides
    with open(CONFIG_PATH, "a") as f:
        f.write(f"\n# Runtime override\n# {key} = {value}\n")
