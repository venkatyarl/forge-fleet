#!/usr/bin/env python3
"""Analyze all ForgeFleet session files.

Reads every .json session, counts messages, tool calls, errors, extracts
the first user message, and writes useful learnings to the brain store.
"""

import json
import sys
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

SESSIONS_DIR = Path("/Users/venkat/.forgefleet/sessions")
LEARNINGS_PATH = Path("/Users/venkat/.forgefleet/brain/learnings.json")

ERROR_INDICATORS = ["error", "traceback", "exception", "failed", "command not found",
                    "no such file", "permission denied", "errno"]


def is_error_tool_result(msg: dict) -> bool:
    """Detect if a tool result message indicates an error."""
    if msg.get("role") != "tool":
        return False
    content = msg.get("content", "")
    if isinstance(content, str):
        lower = content.lower()
        return any(ind in lower for ind in ERROR_INDICATORS)
    return False


def count_tool_calls(msg: dict) -> int:
    """Count tool calls in an assistant message."""
    if msg.get("role") != "assistant":
        return 0
    tc = msg.get("tool_calls")
    if isinstance(tc, list):
        return len(tc)
    return 0


def extract_tool_names(msg: dict) -> list:
    """Extract tool function names from an assistant message."""
    if msg.get("role") != "assistant":
        return []
    tc = msg.get("tool_calls")
    if not isinstance(tc, list):
        return []
    names = []
    for call in tc:
        fn = call.get("function", {})
        name = fn.get("name", "unknown")
        names.append(name)
    return names


def analyze_session(filepath: Path) -> dict | None:
    """Analyze a single session file. Returns stats dict or None on failure."""
    try:
        with open(filepath) as f:
            data = json.load(f)
    except (json.JSONDecodeError, IOError) as e:
        print(f"WARNING: Could not read {filepath.name}: {e}")
        return None

    # Handle both formats: {"messages": [...]} or {"meta": {...}, "messages": [...]}
    if isinstance(data, dict):
        messages = data.get("messages", [])
        meta = data.get("meta", {})
    elif isinstance(data, list):
        messages = data
        meta = {}
    else:
        return None

    if not messages:
        return None

    total_msgs = len(messages)
    tool_call_count = 0
    error_count = 0
    tool_names = []
    first_user_msg = None

    for msg in messages:
        if not isinstance(msg, dict):
            continue
        # First user message
        if first_user_msg is None and msg.get("role") == "user":
            content = msg.get("content", "")
            if isinstance(content, str) and content.strip():
                first_user_msg = content.strip()

        # Tool calls
        tc = count_tool_calls(msg)
        tool_call_count += tc
        tool_names.extend(extract_tool_names(msg))

        # Errors
        if is_error_tool_result(msg):
            error_count += 1

    return {
        "file": filepath.name,
        "session_id": meta.get("session_id", filepath.stem),
        "message_count": total_msgs,
        "tool_call_count": tool_call_count,
        "error_count": error_count,
        "tool_names": tool_names,
        "first_user_msg": first_user_msg,
        "model": meta.get("model"),
        "turn_count": meta.get("turn_count"),
    }


def update_learnings(learnings: list):
    """Read existing learnings.json, append new ones (no duplicates), write back."""
    existing = []
    if LEARNINGS_PATH.exists():
        try:
            with open(LEARNINGS_PATH) as f:
                existing = json.load(f)
        except (json.JSONDecodeError, IOError):
            existing = []

    # Index existing content strings for dedup
    existing_contents = {item.get("content") for item in existing if isinstance(item, dict)}

    added = 0
    for learning in learnings:
        if learning["content"] not in existing_contents:
            existing.append(learning)
            existing_contents.add(learning["content"])
            added += 1

    LEARNINGS_PATH.parent.mkdir(parents=True, exist_ok=True)
    with open(LEARNINGS_PATH, "w") as f:
        json.dump(existing, f, indent=2)

    return added


def main():
    if not SESSIONS_DIR.exists():
        print(f"ERROR: Sessions directory not found: {SESSIONS_DIR}")
        sys.exit(1)

    session_files = sorted(SESSIONS_DIR.glob("*.json"))
    # Exclude non-session files (like dataset files that happen to be there)
    session_files = [f for f in session_files if not f.name.startswith("dataset")]

    print(f"Found {len(session_files)} session files\n")

    results = []
    for sf in session_files:
        r = analyze_session(sf)
        if r:
            results.append(r)

    if not results:
        print("No valid sessions to analyze.")
        sys.exit(0)

    # Aggregate stats
    total_sessions = len(results)
    total_messages = sum(r["message_count"] for r in results)
    total_tool_calls = sum(r["tool_call_count"] for r in results)
    total_errors = sum(r["error_count"] for r in results)
    avg_messages = total_messages / total_sessions

    # Tool frequency
    all_tool_names = []
    for r in results:
        all_tool_names.extend(r["tool_names"])
    tool_freq = Counter(all_tool_names)

    # Sessions with errors
    sessions_with_errors = sum(1 for r in results if r["error_count"] > 0)
    error_rate = total_errors / total_tool_calls if total_tool_calls else 0

    # Message count distribution
    msg_counts = [r["message_count"] for r in results]
    msg_counts.sort()

    print("=== Session Analysis Summary ===")
    print(f"Total sessions analyzed:  {total_sessions}")
    print(f"Total messages:           {total_messages}")
    print(f"Avg messages/session:     {avg_messages:.1f}")
    print(f"Median messages/session:  {msg_counts[len(msg_counts)//2]}")
    print(f"Min / Max messages:       {msg_counts[0]} / {msg_counts[-1]}")
    print(f"Total tool calls:         {total_tool_calls}")
    print(f"Total errors:             {total_errors}")
    print(f"Error rate (errors/tool calls): {error_rate:.2%}")
    print(f"Sessions with errors:     {sessions_with_errors} / {total_sessions}")

    print(f"\nTop 15 most used tools:")
    for name, count in tool_freq.most_common(15):
        print(f"  {name:30s}  {count:5d}")

    # Sample first user messages
    print(f"\nSample first user messages (up to 10):")
    shown = 0
    for r in results:
        if r["first_user_msg"] and shown < 10:
            preview = r["first_user_msg"][:120].replace("\n", " ")
            print(f"  [{r['file'][:20]}] {preview}")
            shown += 1

    # Build learnings
    now = datetime.now(timezone.utc).isoformat()
    learnings = [
        {
            "id": str(uuid4()),
            "category": "session_analysis",
            "content": f"Session analysis ({total_sessions} sessions): avg {avg_messages:.1f} msgs/session, {total_tool_calls} tool calls, {error_rate:.1%} error rate",
            "relevance": 0.8,
            "created_at": now,
            "updated_at": now,
            "source_session": "session_analysis_script",
            "tags": ["analytics", "sessions"],
        },
    ]

    if tool_freq:
        top_tools = ", ".join(f"{n}({c})" for n, c in tool_freq.most_common(5))
        learnings.append({
            "id": str(uuid4()),
            "category": "session_analysis",
            "content": f"Most used tools across sessions: {top_tools}",
            "relevance": 0.8,
            "created_at": now,
            "updated_at": now,
            "source_session": "session_analysis_script",
            "tags": ["analytics", "tools"],
        })

    if error_rate > 0.1:
        learnings.append({
            "id": str(uuid4()),
            "category": "session_analysis",
            "content": f"High error rate detected: {error_rate:.1%}. Most errors in tool execution results.",
            "relevance": 0.9,
            "created_at": now,
            "updated_at": now,
            "source_session": "session_analysis_script",
            "tags": ["analytics", "errors"],
        })

    added = update_learnings(learnings)
    print(f"\nLearnings: {added} new entries written to {LEARNINGS_PATH}")


if __name__ == "__main__":
    main()
