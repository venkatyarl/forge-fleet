#!/usr/bin/env python3
"""Reference implementation: export CLI session JSONL transcripts to Obsidian.

Mirrors the Rust implementation behind `ff session export` and the
`session-export` daemon tick.  Parses `.claude/projects/*.jsonl` (and Codex/Kimi
equivalents), skips system-reminders, marks tool calls as bullets, redacts
tokens/keys, and writes to:

  <vault>/ForgeFleet/sessions/<project-folder>/<YYYY>/<MM-MonthName>/
          <YYYYMMDD>-<computerName>-<sessionID>.md

Same session continuing => append to the same file.  If the file grows past
~900KB, split into a subfolder of part files.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable

PART_SIZE = 900 * 1024

REDACTIONS = [
    (re.compile(r"ghp_[a-zA-Z0-9]{36}"), "github-token"),
    (re.compile(r"github_pat_[a-zA-Z0-9_]{22,}"), "github-pat"),
    (re.compile(r"AGE-SECRET-KEY-[a-zA-Z0-9]{59}"), "age-secret-key"),
    (re.compile(r"sk-ant-[a-zA-Z0-9_-]{32,}"), "anthropic-key"),
    (re.compile(r"ops_[a-zA-Z0-9]{32,}"), "1password-token"),
    (re.compile(r"eyJ[a-zA-Z0-9_-]*\.eyJ[a-zA-Z0-9_-]*\.[a-zA-Z0-9_-]*"), "jwt"),
]

MONTHS = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
]


def redact(text: str) -> str:
    for pattern, label in REDACTIONS:
        text = pattern.sub(f"[REDACTED-{label}]", text)
    return text


def format_entry(obj: dict) -> str | None:
    typ = obj.get("type", "")
    if typ in ("system", "system-reminder", "queue-operation", "last-prompt"):
        return None

    if typ == "user":
        msg = obj.get("message", obj)
        content = msg.get("content", "") if isinstance(msg, dict) else ""
        content = content.strip() if isinstance(content, str) else ""
        if not content:
            return None
        return f"**User:** {content}\n"

    if typ == "assistant":
        msg = obj.get("message", obj)
        out = []
        content = msg.get("content", "") if isinstance(msg, dict) else ""
        if isinstance(content, str) and content.strip():
            out.append(f"**Assistant:** {content.strip()}\n")
        elif isinstance(content, list):
            for part in content:
                if isinstance(part, dict) and "text" in part:
                    out.append(f"**Assistant:** {part['text'].strip()}\n")
        for tc in msg.get("tool_calls", []) if isinstance(msg, dict) else []:
            fn = tc.get("function", tc)
            name = fn.get("name", "tool") if isinstance(fn, dict) else "tool"
            args = fn.get("arguments", "") if isinstance(fn, dict) else ""
            out.append(f"- **Tool call:** `{name}` {args}\n")
        return "".join(out) if out else None

    if typ == "attachment":
        att = obj.get("attachment", obj)
        att_type = att.get("type", "") if isinstance(att, dict) else ""
        if att_type == "tool_result":
            name = att.get("tool_name", att.get("name", "tool"))
            result = att.get("result", att.get("content", ""))
            if not isinstance(result, str):
                result = json.dumps(result)
            return f"- **Tool result:** `{name}` {result}\n"
        if att_type == "tool_use":
            name = att.get("tool_name", att.get("name", "tool"))
            inp = att.get("input", att.get("arguments", ""))
            if not isinstance(inp, str):
                inp = json.dumps(inp)
            return f"- **Tool use:** `{name}` {inp}\n"
        return None

    return None


def parse_ts(obj: dict) -> datetime | None:
    raw = obj.get("timestamp") or obj.get("ts")
    if not raw:
        return None
    try:
        return datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except ValueError:
        return None


def sanitize(name: str) -> str:
    out = []
    for ch in name:
        if ch.isalnum() or ch in "-_":
            out.append(ch)
        elif out and out[-1] != "-":
            out.append("-")
    return "".join(out).strip("-") or "unknown"


def split_text(text: str, max_bytes: int) -> Iterable[str]:
    while text:
        if len(text.encode("utf-8")) <= max_bytes:
            yield text
            return
        pos = max_bytes
        while pos > 0 and not _is_char_boundary(text, pos):
            pos -= 1
        yield text[:pos]
        text = text[pos:]


def _is_char_boundary(text: str, idx: int) -> bool:
    try:
        text[idx]
        return True
    except IndexError:
        return False


def write_parts(parts_dir: Path, base_name: str, existing: str, new_content: str) -> None:
    parts_dir.mkdir(parents=True, exist_ok=True)
    remaining = existing + new_content
    part_index = 1
    while remaining:
        path = parts_dir / f"{base_name}-{part_index}.md"
        if path.exists():
            existing_part = path.read_text(encoding="utf-8")
            if len(existing_part.encode("utf-8")) < PART_SIZE:
                capacity = PART_SIZE - len(existing_part.encode("utf-8"))
                chunk, remaining = _take_bytes(remaining, capacity)
                with path.open("a", encoding="utf-8") as f:
                    f.write(chunk)
                part_index += 1
                continue
        chunk, remaining = _take_bytes(remaining, PART_SIZE)
        path.write_text(chunk, encoding="utf-8")
        part_index += 1


def _take_bytes(text: str, max_bytes: int) -> tuple[str, str]:
    if len(text.encode("utf-8")) <= max_bytes:
        return text, ""
    pos = max_bytes
    while pos > 0 and not _is_char_boundary(text, pos):
        pos -= 1
    return text[:pos], text[pos:]


def write_session_export(
    vault_dir: Path,
    project_folder: str,
    ts: datetime,
    computer_name: str,
    session_id: str,
    new_content: str,
) -> None:
    year = str(ts.year)
    month_dir = f"{ts.month:02d}-{MONTHS[ts.month - 1]}"
    base_name = f"{ts.strftime('%Y%m%d')}-{sanitize(computer_name)}-{sanitize(session_id)}"

    sessions_root = (
        vault_dir
        / "ForgeFleet"
        / "sessions"
        / sanitize(project_folder)
        / year
        / month_dir
    )
    single_file = sessions_root / f"{base_name}.md"
    parts_dir = sessions_root / base_name

    using_parts = parts_dir.is_dir()
    if not using_parts:
        existing_size = single_file.stat().st_size if single_file.exists() else 0
        if existing_size + len(new_content.encode("utf-8")) <= PART_SIZE:
            sessions_root.mkdir(parents=True, exist_ok=True)
            with single_file.open("a", encoding="utf-8") as f:
                f.write(new_content)
            return

        parts_dir.mkdir(parents=True, exist_ok=True)
        existing = single_file.read_text(encoding="utf-8") if single_file.exists() else ""
        if single_file.exists():
            single_file.unlink()
        write_parts(parts_dir, base_name, existing, new_content)
        return

    parts_dir.mkdir(parents=True, exist_ok=True)
    part_files = sorted(p for p in parts_dir.iterdir() if p.suffix == ".md")
    if not part_files:
        write_parts(parts_dir, base_name, "", new_content)
        return

    last = part_files[-1]
    last_index = int(last.stem.rsplit("-", 1)[-1]) if "-" in last.stem else 1
    existing_last = last.read_text(encoding="utf-8")
    if len(existing_last.encode("utf-8")) + len(new_content.encode("utf-8")) <= PART_SIZE:
        with last.open("a", encoding="utf-8") as f:
            f.write(new_content)
    else:
        write_parts(parts_dir, base_name, "", new_content)


def load_cursor(vault_dir: Path) -> dict:
    path = vault_dir / ".ff_session_export_cursor.json"
    if path.exists():
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            pass
    return {"files": {}}


def save_cursor(vault_dir: Path, cursor: dict) -> None:
    path = vault_dir / ".ff_session_export_cursor.json"
    path.write_text(json.dumps(cursor, indent=2), encoding="utf-8")


def process_jsonl(
    path: Path,
    vault_dir: Path,
    project_folder: str,
    computer_name: str,
    cursor: dict,
) -> tuple[bool, int]:
    stat = path.stat()
    mtime = datetime.fromtimestamp(stat.st_mtime, tz=timezone.utc)
    key = str(path)
    prev = cursor["files"].get(key, {})

    if (
        prev.get("mtime_secs", 0) == int(mtime.timestamp())
        and prev.get("bytes_read", 0) >= stat.st_size
    ):
        return False, 0

    start = prev.get("bytes_read", 0)
    if start > stat.st_size:
        start = 0

    rendered = []
    redactions = 0
    session_ts: datetime | None = None

    with path.open("rb") as f:
        f.seek(start)
        for line in f:
            line = line.decode("utf-8", errors="replace")
            if not line.strip():
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            ts = parse_ts(obj)
            if ts and (session_ts is None or ts < session_ts):
                session_ts = ts
            entry = format_entry(obj)
            if entry is None:
                continue
            redacted = redact(entry)
            redactions += redacted.count("[REDACTED-")
            rendered.append(redacted)

    text = "".join(rendered)
    if not text.strip():
        cursor["files"][key] = {
            "mtime_secs": int(mtime.timestamp()),
            "mtime_nanos": 0,
            "bytes_read": stat.st_size,
        }
        return False, redactions

    ts = session_ts or datetime.now(timezone.utc)
    write_session_export(
        vault_dir,
        project_folder,
        ts,
        computer_name,
        path.stem,
        text,
    )

    cursor["files"][key] = {
        "mtime_secs": int(mtime.timestamp()),
        "mtime_nanos": 0,
        "bytes_read": stat.st_size,
    }
    return True, redactions


def export_sessions(vault_dir: Path, source_dirs: list[str], computer_name: str) -> dict:
    vault_dir.mkdir(parents=True, exist_ok=True)
    cursor = load_cursor(vault_dir)
    result = {"files_processed": 0, "sessions_exported": 0, "redactions": 0}

    for raw_dir in source_dirs:
        source_dir = Path(raw_dir).expanduser()
        if not source_dir.is_dir():
            continue
        for path in sorted(source_dir.rglob("*.jsonl")):
            project_folder = (
                path.parent.name if path.parent != source_dir else source_dir.name
            ) or "unknown"
            exported, redactions = process_jsonl(
                path, vault_dir, project_folder, computer_name, cursor
            )
            result["files_processed"] += 1
            if exported:
                result["sessions_exported"] += 1
            result["redactions"] += redactions

    cursor["files"] = {k: v for k, v in cursor["files"].items() if Path(k).exists()}
    save_cursor(vault_dir, cursor)
    return result


def hostname() -> str:
    try:
        return subprocess.check_output(["hostname"]).decode().strip()
    except Exception:
        return "unknown"


def main() -> int:
    parser = argparse.ArgumentParser(description="Export CLI sessions to Obsidian vault")
    parser.add_argument("--vault", help="Vault root (default: ~/projects/Yarli_KnowledgeBase)")
    parser.add_argument("--computer", help="Computer name override")
    parser.add_argument("--source-dir", action="append", help="Source directory (repeatable)")
    parser.add_argument("--dry-run", action="store_true", help="Print what would be exported")
    args = parser.parse_args()

    vault_dir = Path(args.vault).expanduser() if args.vault else Path("~/projects/Yarli_KnowledgeBase").expanduser()
    computer = args.computer or hostname()
    source_dirs = args.source_dir or ["~/.claude/projects", "~/.codex/projects", "~/.kimi/projects"]

    if args.dry_run:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            result = export_sessions(Path(tmp), source_dirs, computer)
            print("! dry run — no files written")
            print(f"  files processed: {result['files_processed']}")
            print(f"  sessions exported: {result['sessions_exported']}")
            print(f"  redactions: {result['redactions']}")
    else:
        result = export_sessions(vault_dir, source_dirs, computer)
        print("✓ session export complete")
        print(f"  files processed: {result['files_processed']}")
        print(f"  sessions exported: {result['sessions_exported']}")
        print(f"  redactions: {result['redactions']}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
