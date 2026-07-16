"""Context Store — FTS5 BM25 search for context compression.

Extracted from Context Mode (MIT license, mksglu/context-mode).
Core pattern: chunk markdown by headings, store in SQLite FTS5,
search via BM25 ranking. 98% token reduction.

Uses Python's built-in sqlite3 (no external dependencies).
"""
import os
import re
import sqlite3
import time
from dataclasses import dataclass, field
from pathlib import Path


# ─── Constants ──────────────────────────────────────────

MAX_CHUNK_BYTES = 4096

STOPWORDS = {
    "the", "and", "for", "are", "but", "not", "you", "all", "can", "had",
    "was", "one", "our", "out", "has", "how", "its", "may", "new", "now",
    "see", "way", "who", "did", "get", "got", "let", "say", "too", "use",
    "will", "with", "this", "that", "from", "they", "been", "have", "many",
    "some", "them", "than", "each", "make", "like", "just", "over", "such",
    "take", "into", "year", "your", "good", "could", "would", "about",
    "which", "their", "there", "other", "after", "should", "through",
    "also", "more", "most", "only", "very", "when", "what", "then",
    "these", "those", "being", "does", "done", "both", "same", "still",
    "while", "where", "here", "were", "much",
    "update", "updates", "updated", "deps", "dev", "tests", "test",
    "add", "added", "fix", "fixed", "run", "running", "using",
}


# ─── Types ──────────────────────────────────────────────

@dataclass
class Chunk:
    title: str
    content: str
    has_code: bool = False


@dataclass
class SearchResult:
    title: str
    content: str
    source: str
    rank: float = 0.0


@dataclass
class IndexResult:
    source: str
    chunks: int
    bytes_indexed: int
    time_ms: float


# ─── Helpers ────────────────────────────────────────────

def sanitize_query(query: str, mode: str = "AND") -> str:
    """Sanitize a search query for FTS5."""
    words = re.sub(r"['\"\(\)\{\}\[\]\*:^~]", " ", query).split()
    words = [
        w for w in words
        if w and w.upper() not in ("AND", "OR", "NOT", "NEAR")
    ]
    if not words:
        return '""'
    joiner = " OR " if mode == "OR" else " "
    return " ".join(f'"{w}"' for w in words)


def chunk_markdown(text: str, max_bytes: int = MAX_CHUNK_BYTES) -> list[Chunk]:
    """Split markdown into chunks by headings, keeping code blocks intact.
    
    Pattern from Context Mode: walk lines, track heading hierarchy,
    start new chunk at each heading. Code fences stay with their section.
    """
    chunks = []
    lines = text.split("\n")
    current_heading = ""
    current_lines = []
    in_code_block = False
    
    for line in lines:
        # Track code fences
        if line.strip().startswith("```"):
            in_code_block = not in_code_block
            current_lines.append(line)
            continue
        
        if in_code_block:
            current_lines.append(line)
            continue
        
        # Check for heading
        if line.startswith("#"):
            # Flush current chunk
            if current_lines:
                content = "\n".join(current_lines).strip()
                if content:
                    has_code = any("```" in l for l in current_lines)
                    chunk = Chunk(title=current_heading, content=content, has_code=has_code)
                    
                    # Split oversized chunks at paragraph boundaries
                    if len(content.encode()) > max_bytes:
                        chunks.extend(_split_large_chunk(chunk, max_bytes))
                    else:
                        chunks.append(chunk)
            
            current_heading = line.strip().lstrip("#").strip()
            current_lines = [line]
        else:
            current_lines.append(line)
    
    # Final chunk
    if current_lines:
        content = "\n".join(current_lines).strip()
        if content:
            has_code = any("```" in l for l in current_lines)
            chunk = Chunk(title=current_heading, content=content, has_code=has_code)
            if len(content.encode()) > max_bytes:
                chunks.extend(_split_large_chunk(chunk, max_bytes))
            else:
                chunks.append(chunk)
    
    return chunks


def _split_large_chunk(chunk: Chunk, max_bytes: int) -> list[Chunk]:
    """Split an oversized chunk at paragraph boundaries."""
    paragraphs = chunk.content.split("\n\n")
    result = []
    current = []
    current_size = 0
    part = 1
    
    for para in paragraphs:
        para_size = len(para.encode())
        if current_size + para_size > max_bytes and current:
            result.append(Chunk(
                title=f"{chunk.title} (part {part})",
                content="\n\n".join(current),
                has_code=chunk.has_code,
            ))
            current = [para]
            current_size = para_size
            part += 1
        else:
            current.append(para)
            current_size += para_size
    
    if current:
        title = f"{chunk.title} (part {part})" if part > 1 else chunk.title
        result.append(Chunk(title=title, content="\n\n".join(current), has_code=chunk.has_code))
    
    return result


def smart_truncate(text: str, max_bytes: int) -> str:
    """Keep head (60%) and tail (40%), snapping to line boundaries.
    
    Pattern from Context Mode: preserve both initial context and
    final error messages.
    """
    if len(text.encode()) <= max_bytes:
        return text
    
    lines = text.split("\n")
    head_budget = int(max_bytes * 0.6)
    tail_budget = max_bytes - head_budget
    
    # Collect head lines
    head = []
    head_bytes = 0
    for line in lines:
        line_bytes = len(line.encode()) + 1
        if head_bytes + line_bytes > head_budget:
            break
        head.append(line)
        head_bytes += line_bytes
    
    # Collect tail lines (reverse)
    tail = []
    tail_bytes = 0
    for line in reversed(lines):
        line_bytes = len(line.encode()) + 1
        if tail_bytes + line_bytes > tail_budget:
            break
        tail.insert(0, line)
        tail_bytes += line_bytes
    
    omitted = len(lines) - len(head) - len(tail)
    separator = f"\n\n... [{omitted} lines omitted] ...\n\n"
    
    return "\n".join(head) + separator + "\n".join(tail)


# ─── Context Store ──────────────────────────────────────

class ContextStore:
    """FTS5-backed knowledge base for context compression.
    
    Usage:
        store = ContextStore()
        store.index("# My doc\\n\\nSome content", "my-doc")
        results = store.search("content query")
        store.close()
    
    All data stored in SQLite — no external services needed.
    Python's sqlite3 has FTS5 built in.
    """
    
    def __init__(self, db_path: str = ""):
        if not db_path:
            db_dir = os.path.expanduser("~/.forgefleet")
            os.makedirs(db_dir, exist_ok=True)
            db_path = os.path.join(db_dir, "context.db")
        
        self.db_path = db_path
        self.db = sqlite3.connect(db_path)
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA synchronous=NORMAL")
        self._init_schema()
    
    def _init_schema(self):
        """Create FTS5 tables if they don't exist."""
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS sources (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                label TEXT NOT NULL,
                indexed_at TEXT NOT NULL DEFAULT (datetime('now')),
                chunk_count INTEGER DEFAULT 0,
                byte_count INTEGER DEFAULT 0
            );
            
            CREATE VIRTUAL TABLE IF NOT EXISTS chunks USING fts5(
                title,
                content,
                source_id UNINDEXED,
                content_type UNINDEXED,
                tokenize='porter unicode61'
            );
            
            CREATE INDEX IF NOT EXISTS idx_sources_label ON sources(label);
        """)
    
    def index(self, content: str, source: str, content_type: str = "prose") -> IndexResult:
        """Index content into the FTS5 store.
        
        Chunks by markdown headings, stores each chunk with source label.
        Returns stats about what was indexed.
        """
        start = time.time()
        
        chunks = chunk_markdown(content)
        if not chunks:
            chunks = [Chunk(title=source, content=content)]
        
        # Insert source
        cursor = self.db.execute(
            "INSERT INTO sources (label, chunk_count, byte_count) VALUES (?, ?, ?)",
            (source, len(chunks), len(content.encode())),
        )
        source_id = cursor.lastrowid
        
        # Insert chunks
        for chunk in chunks:
            ct = "code" if chunk.has_code else content_type
            self.db.execute(
                "INSERT INTO chunks (title, content, source_id, content_type) VALUES (?, ?, ?, ?)",
                (chunk.title, chunk.content, source_id, ct),
            )
        
        self.db.commit()
        
        return IndexResult(
            source=source,
            chunks=len(chunks),
            bytes_indexed=len(content.encode()),
            time_ms=round((time.time() - start) * 1000, 1),
        )
    
    def index_file(self, filepath: str, source: str = "") -> IndexResult:
        """Index a file's contents."""
        if not source:
            source = os.path.basename(filepath)
        content = Path(filepath).read_text()
        ct = "code" if filepath.endswith((".rs", ".py", ".ts", ".tsx", ".js", ".toml")) else "prose"
        return self.index(content, source, ct)
    
    def index_directory(self, dir_path: str, extensions: tuple = (".rs", ".tsx", ".ts", ".py", ".toml", ".md"),
                        source_prefix: str = "") -> list[IndexResult]:
        """Index all matching files in a directory."""
        results = []
        exclude = {"target", "node_modules", ".git", "dist", ".next", "__pycache__", ".venv"}
        
        for root, dirs, files in os.walk(dir_path):
            dirs[:] = [d for d in dirs if d not in exclude]
            for f in files:
                if any(f.endswith(ext) for ext in extensions):
                    filepath = os.path.join(root, f)
                    rel_path = os.path.relpath(filepath, dir_path)
                    source = f"{source_prefix}{rel_path}" if source_prefix else rel_path
                    try:
                        result = self.index_file(filepath, source)
                        results.append(result)
                    except Exception:
                        pass
        
        return results
    
    def search(self, query: str, limit: int = 5, source_filter: str = "",
               content_type: str = "") -> list[SearchResult]:
        """Search indexed content using BM25 ranking.
        
        Returns the most relevant chunks for the query.
        """
        fts_query = sanitize_query(query)
        
        if source_filter:
            rows = self.db.execute("""
                SELECT chunks.title, chunks.content, sources.label,
                       bm25(chunks, 5.0, 1.0) AS rank
                FROM chunks
                JOIN sources ON sources.id = chunks.source_id
                WHERE chunks MATCH ? AND sources.label LIKE ?
                ORDER BY rank
                LIMIT ?
            """, (fts_query, f"%{source_filter}%", limit)).fetchall()
        else:
            rows = self.db.execute("""
                SELECT chunks.title, chunks.content, sources.label,
                       bm25(chunks, 5.0, 1.0) AS rank
                FROM chunks
                JOIN sources ON sources.id = chunks.source_id
                WHERE chunks MATCH ?
                ORDER BY rank
                LIMIT ?
            """, (fts_query, limit)).fetchall()
        
        return [
            SearchResult(title=r[0], content=r[1], source=r[2], rank=r[3])
            for r in rows
        ]
    
    def stats(self) -> dict:
        """Get store statistics."""
        sources = self.db.execute("SELECT COUNT(*) FROM sources").fetchone()[0]
        chunks = self.db.execute("SELECT COUNT(*) FROM chunks").fetchone()[0]
        total_bytes = self.db.execute("SELECT COALESCE(SUM(byte_count), 0) FROM sources").fetchone()[0]
        
        return {
            "sources": sources,
            "chunks": chunks,
            "total_bytes": total_bytes,
            "db_path": self.db_path,
            "db_size_mb": round(os.path.getsize(self.db_path) / 1024 / 1024, 2) if os.path.exists(self.db_path) else 0,
        }
    
    def clear(self):
        """Clear all indexed content."""
        self.db.execute("DELETE FROM chunks")
        self.db.execute("DELETE FROM sources")
        self.db.commit()
    
    def close(self):
        """Close the database connection."""
        self.db.close()
