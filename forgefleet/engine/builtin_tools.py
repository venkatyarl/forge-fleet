"""Built-in ForgeFleet tools for autonomous task execution.

These are intentionally local/self-contained so ForgeFleet can keep working
without a hard dependency on OpenClaw for common execution tasks.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Optional

from .tool import Tool
from .web_research import WebResearcher


def _safe_path(path: str, base_dir: str = ".") -> Path:
    p = Path(path)
    if not p.is_absolute():
        p = Path(base_dir) / p
    return p.resolve()


def read_file(path: str, base_dir: str = ".", max_chars: int = 12000) -> str:
    p = _safe_path(path, base_dir)
    if not p.exists():
        return f"File not found: {p}"
    if p.is_dir():
        return f"Path is a directory, not a file: {p}"
    text = p.read_text(errors="ignore")
    if len(text) > max_chars:
        return text[:max_chars] + f"\n\n... [truncated at {max_chars} chars]"
    return text


def write_file(path: str, content: str, base_dir: str = ".") -> str:
    p = _safe_path(path, base_dir)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content)
    return f"Wrote {len(content)} chars to {p}"


def append_file(path: str, content: str, base_dir: str = ".") -> str:
    p = _safe_path(path, base_dir)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("a") as f:
        f.write(content)
    return f"Appended {len(content)} chars to {p}"


def list_files(path: str = ".", base_dir: str = ".", max_entries: int = 200) -> str:
    p = _safe_path(path, base_dir)
    if not p.exists():
        return f"Path not found: {p}"
    if p.is_file():
        return str(p)
    entries = []
    for i, child in enumerate(sorted(p.iterdir(), key=lambda c: c.name.lower())):
        if i >= max_entries:
            entries.append("... [truncated]")
            break
        kind = "/" if child.is_dir() else ""
        entries.append(child.name + kind)
    return "\n".join(entries)


def search_in_files(query: str, path: str = ".", base_dir: str = ".", max_hits: int = 100) -> str:
    root = _safe_path(path, base_dir)
    if not root.exists():
        return f"Path not found: {root}"
    hits = []
    for file in root.rglob("*"):
        if len(hits) >= max_hits:
            break
        if file.is_dir():
            continue
        if any(part in file.parts for part in (".git", "node_modules", "target", "dist", "build")):
            continue
        try:
            text = file.read_text(errors="ignore")
        except Exception:
            continue
        if query.lower() in text.lower():
            rel = file.relative_to(root)
            hits.append(str(rel))
    return "\n".join(hits) if hits else f"No matches for: {query}"


def get_builtin_tools(base_dir: str = ".") -> list[Tool]:
    web = WebResearcher()
    return [
        Tool(
            name="read_file",
            description="Read a text file from the working directory.",
            parameters={
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to read"}
                },
                "required": ["path"],
            },
            func=lambda path="": read_file(path, base_dir=base_dir),
        ),
        Tool(
            name="write_file",
            description="Write content to a file, creating parent directories if needed.",
            parameters={
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to write"},
                    "content": {"type": "string", "description": "Content to write"},
                },
                "required": ["path", "content"],
            },
            func=lambda path="", content="": write_file(path, content, base_dir=base_dir),
        ),
        Tool(
            name="append_file",
            description="Append content to a file.",
            parameters={
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to append to"},
                    "content": {"type": "string", "description": "Content to append"},
                },
                "required": ["path", "content"],
            },
            func=lambda path="", content="": append_file(path, content, base_dir=base_dir),
        ),
        Tool(
            name="list_files",
            description="List files/directories in a path.",
            parameters={
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory path to list"}
                },
            },
            func=lambda path=".": list_files(path, base_dir=base_dir),
        ),
        Tool(
            name="search_in_files",
            description="Search for text in files under a directory tree.",
            parameters={
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search string"},
                    "path": {"type": "string", "description": "Root path to search"},
                },
                "required": ["query"],
            },
            func=lambda query="", path=".": search_in_files(query, path, base_dir=base_dir),
        ),
        *web.as_tools(),
    ]
