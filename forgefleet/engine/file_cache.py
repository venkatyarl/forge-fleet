"""File Cache — don't re-read unchanged files.

Item #9: Agents re-read the same files every iteration.
Cache file contents with mtime-based invalidation.
"""
import os
import hashlib
from dataclasses import dataclass, field


@dataclass
class CachedFile:
    """A cached file entry."""
    path: str
    content: str
    mtime: float
    size: int
    hash: str


class FileCache:
    """Cache file contents with automatic invalidation.
    
    Checks mtime before returning cached content.
    If file changed on disk, re-reads and updates cache.
    """
    
    def __init__(self, repo_dir: str, max_entries: int = 500):
        self.repo_dir = repo_dir
        self.max_entries = max_entries
        self._cache: dict[str, CachedFile] = {}
        self._access_order: list[str] = []
        self.hits = 0
        self.misses = 0
    
    def read(self, filepath: str) -> str:
        """Read a file, using cache if unchanged."""
        full_path = os.path.join(self.repo_dir, filepath)
        
        if not os.path.exists(full_path):
            return ""
        
        stat = os.stat(full_path)
        cached = self._cache.get(filepath)
        
        if cached and cached.mtime == stat.st_mtime and cached.size == stat.st_size:
            self.hits += 1
            self._touch(filepath)
            return cached.content
        
        # Cache miss — read from disk
        self.misses += 1
        content = open(full_path).read()
        
        self._cache[filepath] = CachedFile(
            path=filepath,
            content=content,
            mtime=stat.st_mtime,
            size=stat.st_size,
            hash=hashlib.md5(content.encode()).hexdigest(),
        )
        self._touch(filepath)
        self._evict()
        
        return content
    
    def invalidate(self, filepath: str):
        """Invalidate a cached file (e.g., after writing to it)."""
        self._cache.pop(filepath, None)
    
    def invalidate_all(self):
        """Clear the entire cache."""
        self._cache.clear()
        self._access_order.clear()
    
    def is_changed(self, filepath: str) -> bool:
        """Check if a file has changed since last read."""
        full_path = os.path.join(self.repo_dir, filepath)
        if not os.path.exists(full_path):
            return filepath in self._cache
        
        stat = os.stat(full_path)
        cached = self._cache.get(filepath)
        
        if not cached:
            return True
        
        return cached.mtime != stat.st_mtime or cached.size != stat.st_size
    
    def changed_files(self) -> list[str]:
        """Get list of cached files that have changed on disk."""
        changed = []
        for filepath in list(self._cache.keys()):
            if self.is_changed(filepath):
                changed.append(filepath)
        return changed
    
    def _touch(self, filepath: str):
        """Move file to end of access order (most recently used)."""
        if filepath in self._access_order:
            self._access_order.remove(filepath)
        self._access_order.append(filepath)
    
    def _evict(self):
        """Evict least recently used entries if cache is full."""
        while len(self._cache) > self.max_entries:
            if self._access_order:
                oldest = self._access_order.pop(0)
                self._cache.pop(oldest, None)
            else:
                break
    
    def stats(self) -> dict:
        total = self.hits + self.misses
        return {
            "entries": len(self._cache),
            "hits": self.hits,
            "misses": self.misses,
            "hit_rate": f"{self.hits/total*100:.0f}%" if total else "N/A",
            "memory_kb": sum(len(c.content) for c in self._cache.values()) // 1024,
        }
