"""Few-Shot Examples — inject good examples before asking LLM to generate.

Dramatically improves local LLM quality by showing what GOOD output looks like.
"""
import json
import os
import sqlite3
from dataclasses import dataclass, field


@dataclass
class Example:
    """A few-shot example of good code."""
    task_type: str
    task_description: str
    output: str
    quality_score: float = 1.0


class FewShotStore:
    """Store and retrieve few-shot examples for LLM prompts.
    
    When an agent produces output that passes validation + review,
    save it as a few-shot example for future tasks of the same type.
    """
    
    def __init__(self, db_path: str = ""):
        if not db_path:
            db_dir = os.path.expanduser("~/.forgefleet")
            os.makedirs(db_dir, exist_ok=True)
            db_path = os.path.join(db_dir, "few_shot.db")
        
        self.db = sqlite3.connect(db_path)
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS examples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_type TEXT NOT NULL,
                task_description TEXT NOT NULL,
                output TEXT NOT NULL,
                quality_score REAL DEFAULT 1.0,
                uses INTEGER DEFAULT 0,
                created_at TEXT DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_examples_type ON examples(task_type);
        """)
    
    def store(self, example: Example):
        """Store a good example."""
        self.db.execute(
            "INSERT INTO examples (task_type, task_description, output, quality_score) VALUES (?, ?, ?, ?)",
            (example.task_type, example.task_description, example.output[:10000], example.quality_score),
        )
        self.db.commit()
    
    def get_examples(self, task_type: str, limit: int = 2) -> list[Example]:
        """Get the best examples for a task type."""
        rows = self.db.execute(
            "SELECT task_type, task_description, output, quality_score FROM examples WHERE task_type = ? ORDER BY quality_score DESC, uses ASC LIMIT ?",
            (task_type, limit),
        ).fetchall()
        
        # Record usage
        for r in rows:
            self.db.execute("UPDATE examples SET uses = uses + 1 WHERE task_type = ? AND task_description = ?",
                          (r[0], r[1]))
        self.db.commit()
        
        return [Example(task_type=r[0], task_description=r[1], output=r[2], quality_score=r[3]) for r in rows]
    
    def inject_into_prompt(self, task_type: str, task_description: str) -> str:
        """Build few-shot context to inject into the prompt."""
        examples = self.get_examples(task_type)
        if not examples:
            return ""
        
        lines = ["## Examples of good output for this type of task:\n"]
        for i, ex in enumerate(examples, 1):
            lines.append(f"### Example {i}: {ex.task_description[:60]}")
            lines.append(f"```\n{ex.output[:3000]}\n```\n")
        
        lines.append("Now produce similar quality output for your task.\n")
        return "\n".join(lines)
    
    def stats(self) -> dict:
        total = self.db.execute("SELECT COUNT(*) FROM examples").fetchone()[0]
        by_type = dict(self.db.execute("SELECT task_type, COUNT(*) FROM examples GROUP BY task_type").fetchall())
        return {"total": total, "by_type": by_type}
    
    def close(self):
        self.db.close()
