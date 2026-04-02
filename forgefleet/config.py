"""ForgeFleet Configuration — reads fleet.toml, hot-reloads on change.

Single file: ~/.forgefleet/fleet.toml
Human-friendly TOML format with comments.
Auto-reloads when file changes — no restart needed.
"""
from __future__ import annotations

import copy
import json
import logging
import os
import socket
import stat
import threading
import time
import tomllib  # Built into Python 3.11+


logger = logging.getLogger(__name__)


CONFIG_PATH = os.path.expanduser(
    os.environ.get("FORGEFLEET_CONFIG_PATH", "~/.forgefleet/fleet.toml")
)
_cache = {}
_cache_mtime = 0
_cache_lock = threading.Lock()


def _is_path_secure(path: str) -> bool:
    """Basic hardening for config file path and permissions."""
    try:
        st = os.stat(path)
    except OSError:
        return False

    if not stat.S_ISREG(st.st_mode):
        logger.warning("Config path is not a regular file: %s", path)
        return False

    if os.name != "nt" and (st.st_mode & (stat.S_IWGRP | stat.S_IWOTH)):
        logger.warning("Refusing insecure config file permissions (group/world writable): %s", path)
        return False

    return True


def _coerce_env_value(value: str):
    """Parse env overrides as TOML scalars/arrays when possible."""
    text = value.strip()
    if not text:
        return ""
    try:
        parsed = tomllib.loads(f"v = {text}")
        return parsed["v"]
    except Exception:
        return value


def _ensure_key_path(key: str):
    if not key or not isinstance(key, str):
        raise ValueError("Config key must be a non-empty string")
    allowed = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-")
    if any(ch not in allowed for ch in key):
        raise ValueError(f"Invalid config key: {key}")


def _toml_key(key: str) -> str:
    if key and all(ch.isalnum() or ch in "_-" for ch in key):
        return key
    return json.dumps(key)


def _toml_value(value) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, list):
        return "[" + ", ".join(_toml_value(v) for v in value) + "]"
    raise TypeError(f"Unsupported config value type: {type(value).__name__}")


def _dump_toml(data: dict) -> str:
    """Minimal TOML serializer for nested dict + scalar/list values."""
    lines: list[str] = []

    def emit_table(table: dict, prefix: str = ""):
        scalar_items = []
        nested_items = []

        for key in sorted(table.keys()):
            value = table[key]
            if isinstance(value, dict):
                nested_items.append((key, value))
            else:
                scalar_items.append((key, value))

        for key, value in scalar_items:
            lines.append(f"{_toml_key(key)} = {_toml_value(value)}")

        for key, value in nested_items:
            section = f"{prefix}.{key}" if prefix else key
            if lines and lines[-1] != "":
                lines.append("")
            lines.append(f"[{section}]")
            emit_table(value, section)

    emit_table(data)
    return "\n".join(lines).strip() + "\n"


def _parse_set_value(value):
    if not isinstance(value, str):
        return value
    text = value.strip()
    if not text:
        return ""
    try:
        return tomllib.loads(f"v = {text}")["v"]
    except Exception:
        return value


def _load() -> dict:
    """Load fleet.toml with caching. Reloads if file changed."""
    global _cache, _cache_mtime

    config_path = CONFIG_PATH
    if not os.path.exists(config_path):
        return copy.deepcopy(_cache) if _cache else {}

    if not _is_path_secure(config_path):
        return copy.deepcopy(_cache) if _cache else {}

    mtime = os.path.getmtime(config_path)
    with _cache_lock:
        if mtime != _cache_mtime:
            try:
                with open(config_path, "rb") as handle:
                    loaded = tomllib.load(handle)
                if isinstance(loaded, dict):
                    _cache = loaded
                    _cache_mtime = mtime
                else:
                    logger.warning("Config root must be a TOML table: %s", config_path)
            except Exception as exc:
                logger.warning("Config load error (%s): %s", config_path, exc)

        return copy.deepcopy(_cache)


def get(key: str, default=None):
    """Get a config value. Supports dot notation: 'notifications.telegram.chat_id'"""
    # Check env var first
    env_key = f"FORGEFLEET_{key.upper().replace('.', '_')}"
    env_val = os.environ.get(env_key)
    if env_val is not None:
        return _coerce_env_value(env_val)

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

def get_ports() -> dict:
    return get("ports", {})

def get_scheduling() -> dict:
    return get("scheduling", {})

def get_canonical_writer() -> str:
    return get("scheduling.canonical_writer", get_gateway_node() or "taylor")

def get_node_capabilities(name: str) -> dict:
    return get_node(name).get("capabilities", {})

def get_node_preferences(name: str) -> dict:
    return get_node(name).get("preferences", {})

def get_node_resources(name: str) -> dict:
    return get_node(name).get("resources", {})

def get_mcp_topology() -> dict:
    return get("mcp", {})

def get_enrollment() -> dict:
    return get("enrollment", {})

def get_bootstrap_targets() -> list:
    return get("bootstrap_targets", [])

def get_database() -> dict:
    return get("database", {})


def _is_production_env() -> bool:
    env = (
        os.environ.get("FORGEFLEET_ENV")
        or os.environ.get("FORGEFLEET_ENVIRONMENT")
        or os.environ.get("ENV")
        or os.environ.get("ENVIRONMENT")
        or ""
    )
    return env.lower() in {"prod", "production"}


def get_database_url() -> str:
    env_url = os.environ.get("FORGEFLEET_DATABASE_URL", "").strip()
    if env_url:
        return env_url

    if _is_production_env():
        raise RuntimeError("FORGEFLEET_DATABASE_URL must be set in production")

    db = get_database()
    config_url = str(db.get("url", "")).strip()
    if config_url:
        return config_url

    host = db.get("host", "127.0.0.1")
    port = db.get("port", 5432)
    name = db.get("name", "forgefleet")
    user = str(db.get("user", "")).strip()
    password = str(db.get("password", "")).strip()

    auth = ""
    if user and password:
        auth = f"{user}:{password}@"
    elif user:
        auth = f"{user}@"

    return f"postgresql://{auth}{host}:{port}/{name}"

def get_node_models(name: str) -> dict:
    return get_node(name).get("models", {})

def get_all_models() -> list[dict]:
    models = []
    for node_name, node in get_nodes().items():
        for model_key, model in node.get("models", {}).items():
            models.append({
                "key": model_key,
                "node": node_name,
                "ip": node.get("ip", "127.0.0.1"),
                **model,
            })
    return models

def get_tier_timeout(tier: int) -> int:
    return get(f"llm.timeouts.tier{tier}", 300)

def get_local_ip() -> str:
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except Exception:
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
    """Set a value in fleet.toml using an atomic write.

    Accepts TOML literals via string values (e.g. "true", "42", "['a']").
    """
    global _cache, _cache_mtime

    _ensure_key_path(key)
    parsed_value = _parse_set_value(value)
    config_path = CONFIG_PATH

    parent_dir = os.path.dirname(config_path) or "."
    os.makedirs(parent_dir, mode=0o700, exist_ok=True)

    config_data = _load()
    if not isinstance(config_data, dict):
        config_data = {}

    current = config_data
    parts = key.split(".")
    for part in parts[:-1]:
        existing = current.get(part)
        if not isinstance(existing, dict):
            existing = {}
            current[part] = existing
        current = existing
    current[parts[-1]] = parsed_value

    rendered = _dump_toml(config_data)
    tmp_path = f"{config_path}.tmp.{os.getpid()}.{int(time.time() * 1000)}"
    with open(tmp_path, "w", encoding="utf-8") as handle:
        handle.write(rendered)

    if os.name != "nt":
        os.chmod(tmp_path, 0o600)

    os.replace(tmp_path, config_path)

    if os.name != "nt":
        os.chmod(config_path, 0o600)

    with _cache_lock:
        _cache = copy.deepcopy(config_data)
        try:
            _cache_mtime = os.path.getmtime(config_path)
        except OSError:
            _cache_mtime = 0
