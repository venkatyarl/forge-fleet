#!/usr/bin/env python3
"""Build a task classification dataset for training a 1M parameter router model.

Reads the raw session dataset, extracts the first user message from each conversation,
classifies it via keyword matching, and writes labeled examples to router-dataset.jsonl.
"""

import json
import re
import sys
from collections import Counter
from pathlib import Path

INPUT_PATH = Path("/Users/venkat/.forgefleet/training_data/dataset.jsonl")
OUTPUT_PATH = Path("/Users/venkat/.forgefleet/training_data/router-dataset.jsonl")

# Classification rules: (label, keywords)  -- checked in priority order
RULES = [
    ("coding", ["write", "create", "implement", "fix", "build", "refactor", "function", "component", "bug"]),
    ("fleet_op", ["ssh", "deploy", "fleet", "node", "restart", "install", "server", "computer"]),
    ("research", ["research", "find out", "what is", "compare", "search", "how does"]),
    ("review", ["review", "check the code", "audit", "security", "analyze"]),
]


def classify(text: str) -> str:
    lower = text.lower()
    for label, keywords in RULES:
        for kw in keywords:
            if kw in lower:
                return label
    # Simple question heuristic
    if len(text) < 50 and text.strip().endswith("?"):
        return "simple_question"
    return "complex"


def extract_first_user_message(messages):
    """Return text of the first user message, or None."""
    for msg in messages:
        if msg.get("role") == "user":
            content = msg.get("content", "")
            if isinstance(content, str):
                return content
            # Handle list-of-parts format (e.g. OpenAI vision style)
            if isinstance(content, list):
                texts = [p.get("text", "") for p in content if isinstance(p, dict) and p.get("type") == "text"]
                return " ".join(texts) if texts else None
    return None


def main():
    if not INPUT_PATH.exists():
        print(f"ERROR: Input file not found: {INPUT_PATH}")
        sys.exit(1)

    counts = Counter()
    written = 0
    skipped = 0

    with open(INPUT_PATH) as fin, open(OUTPUT_PATH, "w") as fout:
        for line_num, line in enumerate(fin, 1):
            line = line.strip()
            if not line:
                continue
            try:
                data = json.loads(line)
            except json.JSONDecodeError as e:
                print(f"WARNING: Skipping malformed JSON on line {line_num}: {e}")
                skipped += 1
                continue

            messages = data.get("messages")
            if not messages or not isinstance(messages, list):
                print(f"WARNING: Skipping line {line_num}: no 'messages' array found")
                skipped += 1
                continue

            user_text = extract_first_user_message(messages)
            if not user_text or not user_text.strip():
                print(f"WARNING: Skipping line {line_num}: no user message found")
                skipped += 1
                continue

            label = classify(user_text)
            counts[label] += 1
            fout.write(json.dumps({"input": user_text, "label": label}) + "\n")
            written += 1

    print(f"\n=== Router Dataset Build Complete ===")
    print(f"Input:   {INPUT_PATH}")
    print(f"Output:  {OUTPUT_PATH}")
    print(f"Written: {written}  |  Skipped: {skipped}")
    print(f"\nLabel distribution:")
    for label, count in sorted(counts.items(), key=lambda x: -x[1]):
        pct = 100.0 * count / written if written else 0
        print(f"  {label:20s}  {count:5d}  ({pct:5.1f}%)")


if __name__ == "__main__":
    main()
