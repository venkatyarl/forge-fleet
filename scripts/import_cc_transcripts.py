#!/usr/bin/env python3
"""
Import Claude Code JSONL transcripts into ForgeFleet ChatML training format.

Scans ~/.claude/projects/ for .jsonl conversation transcripts, converts them
to OpenAI ChatML fine-tuning format with tool call support, and exports as
a training dataset for MLX LoRA fine-tuning.

Usage:
    python3 scripts/import_cc_transcripts.py [--max-turns 20] [--max-tool-result 2000]
"""

import argparse
import hashlib
import json
import os
import sys
import uuid
from pathlib import Path
from datetime import datetime

# Defaults
CLAUDE_PROJECTS_DIR = Path.home() / ".claude" / "projects"
TRAINING_DATA_DIR = Path.home() / ".forgefleet" / "training_data"
MAX_TURNS_PER_CHUNK = 20
MAX_TOOL_RESULT_CHARS = 2000

# Stats
stats = {
    "files_scanned": 0,
    "files_with_data": 0,
    "conversations": 0,
    "user_messages": 0,
    "assistant_messages": 0,
    "tool_calls": 0,
    "tool_results": 0,
    "thinking_blocks_skipped": 0,
    "training_examples": 0,
    "total_turns": 0,
    "skipped_types": {},
}


def extract_text_from_content(content):
    """Extract text from user message content (string or array of blocks)."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for block in content:
            if isinstance(block, dict):
                if block.get("type") == "text":
                    parts.append(block["text"])
                elif block.get("type") == "tool_result":
                    # This is handled separately as a tool role message
                    pass
            elif isinstance(block, str):
                parts.append(block)
        return "\n".join(parts) if parts else ""
    return str(content)


def extract_tool_results_from_content(content):
    """Extract tool_result blocks from user message content array."""
    results = []
    if not isinstance(content, list):
        return results
    for block in content:
        if isinstance(block, dict) and block.get("type") == "tool_result":
            tool_use_id = block.get("tool_use_id", "")
            result_content = block.get("content", "")
            is_error = block.get("is_error", False)
            # Content can be string or list of blocks
            if isinstance(result_content, list):
                text_parts = []
                for rb in result_content:
                    if isinstance(rb, dict) and rb.get("type") == "text":
                        text_parts.append(rb["text"])
                    elif isinstance(rb, str):
                        text_parts.append(rb)
                result_content = "\n".join(text_parts)
            elif not isinstance(result_content, str):
                result_content = str(result_content)
            # Truncate long tool results
            if len(result_content) > MAX_TOOL_RESULT_CHARS:
                result_content = result_content[:MAX_TOOL_RESULT_CHARS] + "\n... [truncated]"
            results.append({
                "tool_use_id": tool_use_id,
                "content": result_content,
                "is_error": is_error,
            })
    return results


def process_assistant_content(content):
    """Process assistant message content blocks.

    Returns (text, tool_calls) where:
    - text: concatenated text blocks (thinking blocks skipped)
    - tool_calls: list of ChatML tool_call objects
    """
    if isinstance(content, str):
        return content, []

    text_parts = []
    tool_calls = []

    if not isinstance(content, list):
        return str(content), []

    for block in content:
        if not isinstance(block, dict):
            continue
        btype = block.get("type", "")
        if btype == "thinking":
            stats["thinking_blocks_skipped"] += 1
            continue
        elif btype == "text":
            text_parts.append(block.get("text", ""))
        elif btype == "tool_use":
            stats["tool_calls"] += 1
            tool_call = {
                "id": block.get("id", f"call_{uuid.uuid4().hex[:12]}"),
                "type": "function",
                "function": {
                    "name": block.get("name", "unknown"),
                    "arguments": json.dumps(block.get("input", {})),
                },
            }
            tool_calls.append(tool_call)

    return "\n".join(text_parts), tool_calls


def parse_transcript(filepath):
    """Parse a Claude Code JSONL transcript file, streaming line by line.

    Yields (role, message_dict) tuples in conversation order.
    The message_dict is already in ChatML format.
    """
    seen_uuids = set()

    with open(filepath, "r", errors="replace") as f:
        for line_num, line in enumerate(f):
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue

            msg_type = obj.get("type", "")

            # Skip non-message types
            if msg_type not in ("user", "assistant"):
                if msg_type not in ("queue-operation", "last-prompt"):
                    stats["skipped_types"][msg_type] = stats["skipped_types"].get(msg_type, 0) + 1
                continue

            # Deduplicate by uuid (assistant messages can appear in multiple
            # lines as streaming chunks with same uuid)
            msg_uuid = obj.get("uuid", "")
            if msg_uuid in seen_uuids:
                continue
            seen_uuids.add(msg_uuid)

            message = obj.get("message", {})
            content = message.get("content")
            if content is None:
                continue

            if msg_type == "user":
                # Check for tool results first
                tool_results = extract_tool_results_from_content(content)
                if tool_results:
                    stats["tool_results"] += len(tool_results)
                    for tr in tool_results:
                        yield ("tool", {
                            "role": "tool",
                            "tool_call_id": tr["tool_use_id"],
                            "content": tr["content"],
                        })

                # Also extract any text content from user
                text = extract_text_from_content(content)
                if text.strip():
                    stats["user_messages"] += 1
                    yield ("user", {
                        "role": "user",
                        "content": text,
                    })

            elif msg_type == "assistant":
                text, tool_calls = process_assistant_content(content)

                # Skip empty assistant messages (e.g., thinking-only)
                if not text.strip() and not tool_calls:
                    continue

                stats["assistant_messages"] += 1
                msg = {"role": "assistant"}
                if text.strip():
                    msg["content"] = text
                else:
                    msg["content"] = ""
                if tool_calls:
                    msg["tool_calls"] = tool_calls

                yield ("assistant", msg)


def build_conversations(filepath):
    """Build a list of ChatML conversations from a transcript.

    Groups consecutive messages into conversations, splitting on
    large gaps or conversation boundaries.
    """
    messages = list(parse_transcript(filepath))
    if not messages:
        return []

    # Build one flat conversation
    conversation = []
    for role, msg in messages:
        conversation.append(msg)

    if len(conversation) < 2:
        return []

    return [conversation]


def chunk_conversation(conversation, max_turns):
    """Split a conversation into chunks of approximately max_turns messages.

    Keeps tool calls with their results together. A 'turn' is a user or
    assistant message (tool messages don't count as turns).
    """
    chunks = []
    current_chunk = []
    turn_count = 0

    # Extract system message if present
    system_msg = None
    start_idx = 0
    if conversation and conversation[0].get("role") == "system":
        system_msg = conversation[0]
        start_idx = 1

    for msg in conversation[start_idx:]:
        current_chunk.append(msg)
        if msg["role"] in ("user", "assistant"):
            turn_count += 1

        # Check if we should split
        if turn_count >= max_turns and msg["role"] == "assistant" and "tool_calls" not in msg:
            # Good split point: after an assistant response without pending tool calls
            chunk = list(current_chunk)
            if system_msg:
                chunk.insert(0, system_msg)
            chunks.append(chunk)
            current_chunk = []
            turn_count = 0

    # Don't forget the last chunk
    if current_chunk:
        # Only keep if it has at least one exchange
        has_user = any(m["role"] == "user" for m in current_chunk)
        has_assistant = any(m["role"] == "assistant" for m in current_chunk)
        if has_user and has_assistant:
            chunk = list(current_chunk)
            if system_msg:
                chunk.insert(0, system_msg)
            chunks.append(chunk)

    return chunks if chunks else []


def make_example_id(filepath, chunk_idx):
    """Generate a deterministic ID for a training example."""
    h = hashlib.sha256(f"{filepath}:{chunk_idx}".encode()).hexdigest()[:16]
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    return f"{ts}_{h}"


def validate_conversation(messages):
    """Basic validation: ensure the conversation is well-formed for training."""
    if len(messages) < 2:
        return False
    roles = [m["role"] for m in messages]
    # Must have at least one user and one assistant message
    if "user" not in roles or "assistant" not in roles:
        return False
    return True


def process_all_transcripts(max_turns, max_tool_result):
    """Main processing pipeline."""
    global MAX_TOOL_RESULT_CHARS
    MAX_TOOL_RESULT_CHARS = max_tool_result

    TRAINING_DATA_DIR.mkdir(parents=True, exist_ok=True)

    # Find all JSONL files
    jsonl_files = []
    if CLAUDE_PROJECTS_DIR.exists():
        for p in CLAUDE_PROJECTS_DIR.rglob("*.jsonl"):
            if p.is_file():
                jsonl_files.append(p)

    print(f"Found {len(jsonl_files)} JSONL transcript files in {CLAUDE_PROJECTS_DIR}")
    print(f"Training data output: {TRAINING_DATA_DIR}")
    print(f"Max turns per chunk: {max_turns}")
    print(f"Max tool result chars: {max_tool_result}")
    print()

    all_examples = []

    for filepath in sorted(jsonl_files):
        stats["files_scanned"] += 1
        rel_path = filepath.relative_to(CLAUDE_PROJECTS_DIR)

        try:
            conversations = build_conversations(filepath)
        except Exception as e:
            print(f"  ERROR parsing {rel_path}: {e}", file=sys.stderr)
            continue

        if not conversations:
            continue

        stats["files_with_data"] += 1
        file_examples = 0

        for conv_idx, conversation in enumerate(conversations):
            stats["conversations"] += 1
            chunks = chunk_conversation(conversation, max_turns)

            for chunk_idx, chunk in enumerate(chunks):
                if not validate_conversation(chunk):
                    continue

                stats["total_turns"] += len([m for m in chunk if m["role"] in ("user", "assistant")])
                example = {"messages": chunk}
                example_id = make_example_id(str(filepath), f"{conv_idx}_{chunk_idx}")

                # Save individual JSON file
                example_path = TRAINING_DATA_DIR / f"{example_id}.json"
                with open(example_path, "w") as ef:
                    json.dump(example, ef, indent=2)

                all_examples.append(example)
                stats["training_examples"] += 1
                file_examples += 1

        if file_examples > 0:
            print(f"  {rel_path}: {file_examples} example(s)")

    # Export full dataset as JSONL
    dataset_path = TRAINING_DATA_DIR / "dataset.jsonl"
    with open(dataset_path, "w") as df:
        for example in all_examples:
            df.write(json.dumps(example) + "\n")

    return dataset_path


def readiness_check():
    """Check if we have enough training data."""
    dataset_path = TRAINING_DATA_DIR / "dataset.jsonl"
    if not dataset_path.exists():
        return False, "No dataset.jsonl found. Run import first."

    count = 0
    with open(dataset_path) as f:
        for line in f:
            if line.strip():
                count += 1

    if count < 10:
        return False, f"Only {count} examples. Need at least 10 for meaningful training."
    if count < 50:
        return True, f"{count} examples. Minimal but usable. 50+ recommended."
    return True, f"{count} examples. Good dataset size."


def print_stats():
    """Print processing statistics."""
    print()
    print("=" * 60)
    print("IMPORT STATISTICS")
    print("=" * 60)
    print(f"  Files scanned:           {stats['files_scanned']}")
    print(f"  Files with data:         {stats['files_with_data']}")
    print(f"  Conversations found:     {stats['conversations']}")
    print(f"  User messages:           {stats['user_messages']}")
    print(f"  Assistant messages:       {stats['assistant_messages']}")
    print(f"  Tool calls:              {stats['tool_calls']}")
    print(f"  Tool results:            {stats['tool_results']}")
    print(f"  Thinking blocks skipped: {stats['thinking_blocks_skipped']}")
    print(f"  Total turns:             {stats['total_turns']}")
    print(f"  Training examples:       {stats['training_examples']}")
    if stats["skipped_types"]:
        print(f"  Skipped message types:   {stats['skipped_types']}")
    print("=" * 60)

    # Readiness check
    ready, msg = readiness_check()
    status = "READY" if ready else "NOT READY"
    print(f"\n  Training readiness: [{status}] {msg}")

    # Dataset size
    dataset_path = TRAINING_DATA_DIR / "dataset.jsonl"
    if dataset_path.exists():
        size_mb = dataset_path.stat().st_size / (1024 * 1024)
        print(f"  Dataset file: {dataset_path} ({size_mb:.1f} MB)")

    print()


def main():
    parser = argparse.ArgumentParser(
        description="Import Claude Code transcripts into ForgeFleet training format"
    )
    parser.add_argument(
        "--max-turns", type=int, default=MAX_TURNS_PER_CHUNK,
        help=f"Max turns per training example (default: {MAX_TURNS_PER_CHUNK})"
    )
    parser.add_argument(
        "--max-tool-result", type=int, default=MAX_TOOL_RESULT_CHARS,
        help=f"Max chars for tool results (default: {MAX_TOOL_RESULT_CHARS})"
    )
    parser.add_argument(
        "--dry-run", action="store_true",
        help="Parse and report stats without writing files"
    )
    args = parser.parse_args()

    print(f"ForgeFleet Training Data Importer")
    print(f"{'=' * 60}")
    print(f"Started at: {datetime.now().isoformat()}")
    print()

    # Check existing training data
    existing = list(TRAINING_DATA_DIR.glob("*.json")) if TRAINING_DATA_DIR.exists() else []
    print(f"Existing training examples: {len(existing)}")

    if not args.dry_run:
        dataset_path = process_all_transcripts(args.max_turns, args.max_tool_result)
        print(f"\nDataset exported to: {dataset_path}")
    else:
        print("[DRY RUN] Would process transcripts but not write files.")
        process_all_transcripts(args.max_turns, args.max_tool_result)

    print_stats()


if __name__ == "__main__":
    main()
