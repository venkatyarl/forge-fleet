"""Network reachability monitoring."""

from __future__ import annotations

import asyncio
import threading
from typing import Optional

DEFAULT_HOST = "8.8.8.8"
DEFAULT_PORT = 53
DEFAULT_TIMEOUT = 3.0


class NetworkMonitor:
    """Checks network reachability via a lightweight TCP connection probe."""

    def __init__(
        self,
        host: str = DEFAULT_HOST,
        port: int = DEFAULT_PORT,
        timeout: float = DEFAULT_TIMEOUT,
    ) -> None:
        self._host = host
        self._port = port
        self._timeout = timeout
        self._lock = threading.Lock()
        self._cached_status: Optional[bool] = None

    async def is_online(self) -> bool:
        """Attempt a lightweight connection check and return online status."""
        try:
            _, writer = await asyncio.wait_for(
                asyncio.open_connection(self._host, self._port),
                timeout=self._timeout,
            )
        except (OSError, asyncio.TimeoutError):
            online = False
        else:
            online = True
            writer.close()
            try:
                await writer.wait_closed()
            except OSError:
                pass

        with self._lock:
            self._cached_status = online
        return online

    def get_cached_status(self) -> Optional[bool]:
        """Return the last known online status, or None if never checked."""
        with self._lock:
            return self._cached_status
