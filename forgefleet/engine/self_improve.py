"""Self-Improvement Engine — learn from errors, get better over time.

Extracted from self-improving-agent patterns.
Tracks: what worked, what failed, common error patterns,
which models are best at which tasks.

Stores learnings in SQLite alongside the context store.
"""
import json
import os
import sqlite3
import time
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class Learning:
    """A recorded learning from a task execution."""
    task_type: str       # "code_write", "code_review", "research", etc.
    model_used: str      # Model that was used
    tier: int            # Which tier
    outcome: str         # "success" or "failure"
    error_pattern: str = ""  # Common error string
    fix_applied: str = ""    # What fixed it
    task_hash: str = ""      # Hash of task for dedup
    duration_seconds: float = 0
    timestamp: float = 0


class SelfImprover:
    """Learns from successes and failures to improve over time.
    
    Tracks:
    1. Which models are best for which task types
    2. Common error patterns and their fixes
    3. Task duration by model/tier (to optimize routing)
    4. Success rates by task complexity
    
    Uses:
    - Before a task: check if similar task failed before → apply fix preemptively
    - After a task: record outcome for future reference
    - Model selection: route to the model with best success rate for this task type
    """
    
    def __init__(self, db_path: str = ""):
        if not db_path:
            db_dir = os.path.expanduser("~/.forgefleet")
            os.makedirs(db_dir, exist_ok=True)
            db_path = os.path.join(db_dir, "learnings.db")
        
        self.db_path = db_path
        self.db = sqlite3.connect(db_path)
        self.db.execute("PRAGMA journal_mode=WAL")
        self._init_schema()
    
    def _init_schema(self):
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS learnings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_type TEXT NOT NULL,
                model_used TEXT NOT NULL,
                tier INTEGER NOT NULL,
                outcome TEXT NOT NULL,
                error_pattern TEXT DEFAULT '',
                fix_applied TEXT DEFAULT '',
                task_hash TEXT DEFAULT '',
                duration_seconds REAL DEFAULT 0,
                timestamp REAL NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            
            CREATE TABLE IF NOT EXISTS error_fixes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                error_pattern TEXT NOT NULL,
                fix_description TEXT NOT NULL,
                times_applied INTEGER DEFAULT 1,
                success_rate REAL DEFAULT 1.0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            
            CREATE TABLE IF NOT EXISTS model_scores (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                model_name TEXT NOT NULL,
                task_type TEXT NOT NULL,
                total_tasks INTEGER DEFAULT 0,
                successes INTEGER DEFAULT 0,
                avg_duration REAL DEFAULT 0,
                last_updated TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(model_name, task_type)
            );
            
            CREATE INDEX IF NOT EXISTS idx_learnings_type ON learnings(task_type);
            CREATE INDEX IF NOT EXISTS idx_learnings_outcome ON learnings(outcome);
            CREATE INDEX IF NOT EXISTS idx_error_fixes_pattern ON error_fixes(error_pattern);
        """)
    
    def record(self, learning: Learning):
        """Record a learning from a task execution."""
        if not learning.timestamp:
            learning.timestamp = time.time()
        
        self.db.execute(
            """INSERT INTO learnings 
               (task_type, model_used, tier, outcome, error_pattern, fix_applied, task_hash, duration_seconds, timestamp)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)""",
            (learning.task_type, learning.model_used, learning.tier,
             learning.outcome, learning.error_pattern, learning.fix_applied,
             learning.task_hash, learning.duration_seconds, learning.timestamp),
        )
        
        # Update model scores
        self.db.execute(
            """INSERT INTO model_scores (model_name, task_type, total_tasks, successes, avg_duration, last_updated)
               VALUES (?, ?, 1, ?, ?, datetime('now'))
               ON CONFLICT(model_name, task_type) DO UPDATE SET
                   total_tasks = total_tasks + 1,
                   successes = successes + ?,
                   avg_duration = (avg_duration * total_tasks + ?) / (total_tasks + 1),
                   last_updated = datetime('now')""",
            (learning.model_used, learning.task_type,
             1 if learning.outcome == "success" else 0,
             learning.duration_seconds,
             1 if learning.outcome == "success" else 0,
             learning.duration_seconds),
        )
        
        # Record error fix if applicable
        if learning.error_pattern and learning.fix_applied:
            existing = self.db.execute(
                "SELECT id, times_applied FROM error_fixes WHERE error_pattern = ?",
                (learning.error_pattern,)
            ).fetchone()
            
            if existing:
                self.db.execute(
                    "UPDATE error_fixes SET times_applied = times_applied + 1 WHERE id = ?",
                    (existing[0],)
                )
            else:
                self.db.execute(
                    "INSERT INTO error_fixes (error_pattern, fix_description) VALUES (?, ?)",
                    (learning.error_pattern, learning.fix_applied),
                )
        
        self.db.commit()
    
    def get_known_fix(self, error_text: str) -> str:
        """Check if we've seen this error before and know a fix."""
        # Search for matching error patterns
        words = error_text.lower().split()[:5]  # First 5 words as key
        key = " ".join(words)
        
        row = self.db.execute(
            "SELECT fix_description, times_applied, success_rate FROM error_fixes WHERE error_pattern LIKE ? ORDER BY times_applied DESC LIMIT 1",
            (f"%{key}%",)
        ).fetchone()
        
        if row:
            return f"Known fix (applied {row[1]}x, {row[2]*100:.0f}% success): {row[0]}"
        return ""
    
    def best_model_for(self, task_type: str) -> dict:
        """Get the best model for a given task type based on history."""
        rows = self.db.execute(
            """SELECT model_name, total_tasks, successes, avg_duration,
                      CAST(successes AS REAL) / NULLIF(total_tasks, 0) as success_rate
               FROM model_scores
               WHERE task_type = ? AND total_tasks >= 3
               ORDER BY success_rate DESC, avg_duration ASC
               LIMIT 5""",
            (task_type,)
        ).fetchall()
        
        if not rows:
            return {"recommendation": "No data yet — using default routing"}
        
        best = rows[0]
        return {
            "recommendation": best[0],
            "success_rate": f"{best[4]*100:.0f}%",
            "avg_duration": f"{best[3]:.1f}s",
            "total_tasks": best[1],
            "alternatives": [
                {"model": r[0], "rate": f"{r[4]*100:.0f}%", "tasks": r[1]}
                for r in rows[1:]
            ],
        }
    
    def get_error_patterns(self, limit: int = 10) -> list[dict]:
        """Get most common error patterns."""
        rows = self.db.execute(
            """SELECT error_pattern, COUNT(*) as count, 
                      GROUP_CONCAT(DISTINCT model_used) as models
               FROM learnings 
               WHERE outcome = 'failure' AND error_pattern != ''
               GROUP BY error_pattern
               ORDER BY count DESC
               LIMIT ?""",
            (limit,)
        ).fetchall()
        
        return [
            {"error": r[0][:100], "count": r[1], "models": r[2]}
            for r in rows
        ]
    
    def stats(self) -> dict:
        """Get overall learning statistics."""
        total = self.db.execute("SELECT COUNT(*) FROM learnings").fetchone()[0]
        successes = self.db.execute("SELECT COUNT(*) FROM learnings WHERE outcome='success'").fetchone()[0]
        fixes = self.db.execute("SELECT COUNT(*) FROM error_fixes").fetchone()[0]
        models = self.db.execute("SELECT COUNT(DISTINCT model_name) FROM model_scores").fetchone()[0]
        
        return {
            "total_learnings": total,
            "successes": successes,
            "failures": total - successes,
            "success_rate": f"{successes/total*100:.0f}%" if total else "N/A",
            "known_fixes": fixes,
            "models_tracked": models,
        }
    
    def close(self):
        self.db.close()
