"""Hot Reload — detect fleet.toml changes and propagate without restart."""
from __future__ import annotations

import hashlib
import logging
import os
import threading
import time
import tomllib
from dataclasses import dataclass, field

from .. import config


logger = logging.getLogger(__name__)


@dataclass
class ConfigWatcher:
    """Watch fleet.toml for changes and notify listeners with parsed config."""

    config_path: str = ""
    poll_interval: float = 5.0  # Check every 5 seconds
    callbacks: list = field(default_factory=list)
    _last_hash: str = ""
    _running: bool = False
    _thread: threading.Thread | None = None

    def __post_init__(self):
        if not self.config_path:
            self.config_path = config.CONFIG_PATH
        if self.config_path and os.path.exists(self.config_path):
            self._last_hash = self._file_hash()

    def on_change(self, callback):
        """Register a callback for config changes. callback(new_config: dict)"""
        self.callbacks.append(callback)

    def start(self):
        """Start watching in background."""
        if self._running:
            return
        self._running = True
        self._thread = threading.Thread(target=self._watch_loop, daemon=True)
        self._thread.start()

    def stop(self):
        self._running = False

    def _file_hash(self) -> str:
        """Get SHA256 of the config file."""
        try:
            with open(self.config_path, "rb") as handle:
                content = handle.read()
            return hashlib.sha256(content).hexdigest()
        except Exception:
            return ""

    def _load_config(self) -> dict | None:
        try:
            with open(self.config_path, "rb") as handle:
                parsed = tomllib.load(handle)
            if isinstance(parsed, dict):
                return parsed
        except Exception as exc:
            logger.warning("Ignoring invalid hot-reload config update for %s: %s", self.config_path, exc)
        return None

    def _notify_callbacks(self, new_config: dict):
        for cb in self.callbacks:
            try:
                cb(new_config)
            except Exception as exc:
                logger.warning("Config watcher callback failed: %s", exc)

    def _watch_loop(self):
        while self._running:
            time.sleep(self.poll_interval)
            current_hash = self._file_hash()
            if current_hash and current_hash != self._last_hash:
                self._last_hash = current_hash
                loaded = self._load_config()
                if loaded is not None:
                    self._notify_callbacks(loaded)

    def check_now(self) -> bool:
        """Force an immediate check. Returns True if changed."""
        current_hash = self._file_hash()
        if current_hash != self._last_hash:
            self._last_hash = current_hash
            loaded = self._load_config()
            if loaded is not None:
                self._notify_callbacks(loaded)
            return True
        return False
