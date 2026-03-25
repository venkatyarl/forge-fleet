"""SQLite-based shared memory for fleet agents."""
import sqlite3
import json
import time
import os
from pathlib import Path
from typing import Optional
from dataclasses import dataclass


@dataclass
class MemoryEntry:
    task_pattern: str      # e.g., "auth email sending", "billing CRUD"
    outcome: str           # "success" or "failure"
    model_used: str        # e.g., "Qwen2.5-Coder-32B"
    tier_completed: int    # which tier finished the task
    error_pattern: str     # if failed, what error
    code_pattern: str      # snippet of what worked/failed
    node: str              # which node produced this
    timestamp: float


class MemoryStore:
    """SQLite shared memory — stores what worked, what failed, patterns learned.
    
    Each node has a local copy. Syncs periodically via file copy.
    Agents check memory before starting: 'has this been tried before?'
    """
    
    def __init__(self, db_path: Optional[str] = None):
        self.db_path = db_path or str(Path.home() / ".forgefleet" / "memory.db")
        os.makedirs(os.path.dirname(self.db_path), exist_ok=True)
        self._init_db()
    
    def _init_db(self):
        """Create tables if they don't exist."""
        with sqlite3.connect(self.db_path) as conn:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS memories (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    task_pattern TEXT NOT NULL,
                    outcome TEXT NOT NULL,
                    model_used TEXT,
                    tier_completed INTEGER,
                    error_pattern TEXT,
                    code_pattern TEXT,
                    node TEXT,
                    timestamp REAL,
                    metadata TEXT
                )
            """)
            conn.execute("""
                CREATE TABLE IF NOT EXISTS code_patterns (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    pattern_type TEXT NOT NULL,
                    language TEXT,
                    pattern TEXT NOT NULL,
                    description TEXT,
                    success_count INTEGER DEFAULT 0,
                    failure_count INTEGER DEFAULT 0,
                    last_used REAL
                )
            """)
            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_memories_task ON memories(task_pattern)
            """)
            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_memories_outcome ON memories(outcome)
            """)
    
    def remember(self, entry: MemoryEntry, metadata: dict = None):
        """Store a memory from a completed task."""
        with sqlite3.connect(self.db_path) as conn:
            conn.execute(
                """INSERT INTO memories 
                   (task_pattern, outcome, model_used, tier_completed, 
                    error_pattern, code_pattern, node, timestamp, metadata)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (entry.task_pattern, entry.outcome, entry.model_used,
                 entry.tier_completed, entry.error_pattern, entry.code_pattern,
                 entry.node, entry.timestamp or time.time(),
                 json.dumps(metadata) if metadata else None)
            )
    
    def recall(self, task_pattern: str, limit: int = 5) -> list[MemoryEntry]:
        """Recall memories similar to a task pattern."""
        with sqlite3.connect(self.db_path) as conn:
            # Search by keyword matching
            keywords = task_pattern.lower().split()
            conditions = " OR ".join(["task_pattern LIKE ?" for _ in keywords])
            params = [f"%{kw}%" for kw in keywords]
            
            rows = conn.execute(
                f"""SELECT task_pattern, outcome, model_used, tier_completed,
                    error_pattern, code_pattern, node, timestamp
                    FROM memories 
                    WHERE {conditions}
                    ORDER BY timestamp DESC LIMIT ?""",
                params + [limit]
            ).fetchall()
            
            return [MemoryEntry(*row) for row in rows]
    
    def get_best_model(self, task_pattern: str) -> Optional[str]:
        """Get the model that succeeded most on similar tasks."""
        with sqlite3.connect(self.db_path) as conn:
            keywords = task_pattern.lower().split()[:3]
            conditions = " OR ".join(["task_pattern LIKE ?" for _ in keywords])
            params = [f"%{kw}%" for kw in keywords]
            
            row = conn.execute(
                f"""SELECT model_used, COUNT(*) as wins
                    FROM memories 
                    WHERE outcome = 'success' AND ({conditions})
                    GROUP BY model_used
                    ORDER BY wins DESC LIMIT 1""",
                params
            ).fetchone()
            
            return row[0] if row else None
    
    def get_error_patterns(self, limit: int = 10) -> list[dict]:
        """Get common error patterns to avoid."""
        with sqlite3.connect(self.db_path) as conn:
            rows = conn.execute(
                """SELECT error_pattern, COUNT(*) as count, model_used
                   FROM memories 
                   WHERE outcome = 'failure' AND error_pattern != ''
                   GROUP BY error_pattern
                   ORDER BY count DESC LIMIT ?""",
                (limit,)
            ).fetchall()
            
            return [{"error": r[0], "count": r[1], "model": r[2]} for r in rows]
    
    def store_code_pattern(self, pattern_type: str, language: str, 
                           pattern: str, description: str):
        """Store a successful code pattern for reuse."""
        with sqlite3.connect(self.db_path) as conn:
            conn.execute(
                """INSERT OR REPLACE INTO code_patterns 
                   (pattern_type, language, pattern, description, success_count, last_used)
                   VALUES (?, ?, ?, ?, COALESCE(
                       (SELECT success_count + 1 FROM code_patterns 
                        WHERE pattern_type = ? AND language = ?), 1), ?)""",
                (pattern_type, language, pattern, description, 
                 pattern_type, language, time.time())
            )
    
    def get_code_pattern(self, pattern_type: str, language: str = "rust") -> Optional[str]:
        """Get a proven code pattern."""
        with sqlite3.connect(self.db_path) as conn:
            row = conn.execute(
                """SELECT pattern FROM code_patterns 
                   WHERE pattern_type = ? AND language = ?
                   ORDER BY success_count DESC LIMIT 1""",
                (pattern_type, language)
            ).fetchone()
            return row[0] if row else None
    
    def stats(self) -> dict:
        """Get memory statistics."""
        with sqlite3.connect(self.db_path) as conn:
            total = conn.execute("SELECT COUNT(*) FROM memories").fetchone()[0]
            successes = conn.execute("SELECT COUNT(*) FROM memories WHERE outcome='success'").fetchone()[0]
            failures = conn.execute("SELECT COUNT(*) FROM memories WHERE outcome='failure'").fetchone()[0]
            patterns = conn.execute("SELECT COUNT(*) FROM code_patterns").fetchone()[0]
            return {
                "total_memories": total,
                "successes": successes,
                "failures": failures,
                "success_rate": f"{successes/(total or 1)*100:.1f}%",
                "code_patterns": patterns,
            }
