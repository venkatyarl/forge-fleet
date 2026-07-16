"""Pattern Library — store and reuse successful code patterns.

Items #2, #3, #10: Contextual learning + code patterns + cross-project learning.
"How did we implement JWT last time?" → pull from library, adapt to context.
"""
import json
import os
import sqlite3
import time
from dataclasses import dataclass, field


@dataclass
class CodePattern:
    """A successful code pattern worth reusing."""
    name: str
    language: str  # "rust", "typescript", "python"
    category: str  # "auth", "api", "database", "error_handling", etc.
    description: str
    code: str
    project: str = ""  # Which project it came from
    files: list = field(default_factory=list)
    tags: list = field(default_factory=list)
    uses: int = 0
    created_at: float = 0


class PatternLibrary:
    """Store and retrieve successful code patterns across all projects.
    
    When an agent writes code that passes tests, the pattern gets saved.
    Next time a similar task comes up, the pattern is provided as reference.
    Cross-project: JWT pattern from HireFlow360 helps FierceFlow.
    """
    
    def __init__(self, db_path: str = ""):
        if not db_path:
            db_dir = os.path.expanduser("~/.forgefleet")
            os.makedirs(db_dir, exist_ok=True)
            db_path = os.path.join(db_dir, "patterns.db")
        
        self.db = sqlite3.connect(db_path)
        self.db.execute("PRAGMA journal_mode=WAL")
        self._init_schema()
    
    def _init_schema(self):
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS patterns (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                language TEXT NOT NULL,
                category TEXT NOT NULL,
                description TEXT NOT NULL,
                code TEXT NOT NULL,
                project TEXT DEFAULT '',
                files TEXT DEFAULT '[]',
                tags TEXT DEFAULT '[]',
                uses INTEGER DEFAULT 0,
                created_at REAL NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS patterns_fts USING fts5(
                name, description, category, tags,
                content=patterns, content_rowid=id,
                tokenize='porter unicode61'
            );
            CREATE INDEX IF NOT EXISTS idx_patterns_category ON patterns(category);
            CREATE INDEX IF NOT EXISTS idx_patterns_language ON patterns(language);
        """)
    
    def store(self, pattern: CodePattern):
        """Store a new code pattern."""
        if not pattern.created_at:
            pattern.created_at = time.time()
        
        cursor = self.db.execute(
            "INSERT INTO patterns (name, language, category, description, code, project, files, tags, uses, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (pattern.name, pattern.language, pattern.category, pattern.description,
             pattern.code, pattern.project, json.dumps(pattern.files),
             json.dumps(pattern.tags), pattern.uses, pattern.created_at),
        )
        
        # Update FTS index
        self.db.execute(
            "INSERT INTO patterns_fts (rowid, name, description, category, tags) VALUES (?, ?, ?, ?, ?)",
            (cursor.lastrowid, pattern.name, pattern.description, pattern.category,
             " ".join(pattern.tags)),
        )
        self.db.commit()
    
    def find(self, query: str, language: str = "", limit: int = 5) -> list[CodePattern]:
        """Find patterns matching a query."""
        if language:
            rows = self.db.execute("""
                SELECT p.name, p.language, p.category, p.description, p.code,
                       p.project, p.files, p.tags, p.uses, p.created_at
                FROM patterns p
                JOIN patterns_fts f ON f.rowid = p.id
                WHERE patterns_fts MATCH ? AND p.language = ?
                ORDER BY rank
                LIMIT ?
            """, (query, language, limit)).fetchall()
        else:
            rows = self.db.execute("""
                SELECT p.name, p.language, p.category, p.description, p.code,
                       p.project, p.files, p.tags, p.uses, p.created_at
                FROM patterns p
                JOIN patterns_fts f ON f.rowid = p.id
                WHERE patterns_fts MATCH ?
                ORDER BY rank
                LIMIT ?
            """, (query, limit)).fetchall()
        
        return [
            CodePattern(
                name=r[0], language=r[1], category=r[2], description=r[3],
                code=r[4], project=r[5], files=json.loads(r[6] or "[]"),
                tags=json.loads(r[7] or "[]"), uses=r[8], created_at=r[9],
            )
            for r in rows
        ]
    
    def find_by_category(self, category: str, language: str = "") -> list[CodePattern]:
        """Find patterns by category."""
        query = "SELECT * FROM patterns WHERE category = ?"
        params = [category]
        if language:
            query += " AND language = ?"
            params.append(language)
        query += " ORDER BY uses DESC LIMIT 10"
        
        rows = self.db.execute(query, params).fetchall()
        return [self._row_to_pattern(r) for r in rows]
    
    def record_use(self, pattern_id: int):
        """Record that a pattern was used."""
        self.db.execute("UPDATE patterns SET uses = uses + 1 WHERE id = ?", (pattern_id,))
        self.db.commit()
    
    def context_for_task(self, task: str, language: str = "") -> str:
        """Get relevant patterns as context for a task."""
        patterns = self.find(task, language, limit=3)
        if not patterns:
            return ""
        
        lines = ["## Relevant patterns from previous work:\n"]
        for p in patterns:
            lines.append(f"### {p.name} (from {p.project})")
            lines.append(f"Category: {p.category} | Used {p.uses} times")
            lines.append(f"```{p.language}")
            lines.append(p.code[:2000])
            lines.append("```\n")
        
        return "\n".join(lines)
    
    def stats(self) -> dict:
        total = self.db.execute("SELECT COUNT(*) FROM patterns").fetchone()[0]
        by_lang = dict(self.db.execute("SELECT language, COUNT(*) FROM patterns GROUP BY language").fetchall())
        by_cat = dict(self.db.execute("SELECT category, COUNT(*) FROM patterns GROUP BY category ORDER BY COUNT(*) DESC LIMIT 10").fetchall())
        most_used = self.db.execute("SELECT name, uses, project FROM patterns ORDER BY uses DESC LIMIT 5").fetchall()
        
        return {"total": total, "by_language": by_lang, "by_category": by_cat,
                "most_used": [{"name": r[0], "uses": r[1], "project": r[2]} for r in most_used]}
    
    def _row_to_pattern(self, r) -> CodePattern:
        return CodePattern(name=r[1], language=r[2], category=r[3], description=r[4],
                          code=r[5], project=r[6], files=json.loads(r[7] or "[]"),
                          tags=json.loads(r[8] or "[]"), uses=r[9], created_at=r[10])
    
    def close(self):
        self.db.close()
