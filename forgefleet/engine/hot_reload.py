"""Hot Reload — detect fleet.json changes and propagate without restart.

Item #6: When fleet.json is edited, all components automatically pick up
the new configuration without needing a daemon restart.
"""
import json
import os
import time
import hashlib
import threading
from dataclasses import dataclass, field


@dataclass
class ConfigWatcher:
    """Watch fleet.json for changes and notify listeners.
    
    Computes SHA256 of the file every N seconds.
    If hash changes, calls all registered callbacks with new config.
    """
    config_path: str = ""
    poll_interval: float = 5.0  # Check every 5 seconds
    callbacks: list = field(default_factory=list)
    _last_hash: str = ""
    _running: bool = False
    _thread: threading.Thread = None
    
    def __post_init__(self):
        if not self.config_path:
            for p in [
                os.path.expanduser("~/fleet.json"),
                os.path.expanduser("~/.openclaw/workspace/fleet.json"),
            ]:
                if os.path.exists(p):
                    self.config_path = p
                    break
        if self.config_path and os.path.exists(self.config_path):
            self._last_hash = self._file_hash()
    
    def on_change(self, callback):
        """Register a callback for config changes. callback(new_config: dict)"""
        self.callbacks.append(callback)
    
    def start(self):
        """Start watching in background."""
        self._running = True
        self._thread = threading.Thread(target=self._watch_loop, daemon=True)
        self._thread.start()
    
    def stop(self):
        self._running = False
    
    def _file_hash(self) -> str:
        """Get SHA256 of the config file."""
        try:
            content = open(self.config_path, "rb").read()
            return hashlib.sha256(content).hexdigest()
        except Exception:
            return ""
    
    def _watch_loop(self):
        while self._running:
            time.sleep(self.poll_interval)
            current_hash = self._file_hash()
            if current_hash and current_hash != self._last_hash:
                self._last_hash = current_hash
                try:
                    with open(self.config_path) as f:
                        config = json.load(f)
                    for cb in self.callbacks:
                        try:
                            cb(config)
                        except Exception:
                            pass
                except Exception:
                    pass
    
    def check_now(self) -> bool:
        """Force an immediate check. Returns True if changed."""
        current_hash = self._file_hash()
        if current_hash != self._last_hash:
            self._last_hash = current_hash
            try:
                with open(self.config_path) as f:
                    config = json.load(f)
                for cb in self.callbacks:
                    cb(config)
            except Exception:
                pass
            return True
        return False
