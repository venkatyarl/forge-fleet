"""File operations for the agent loop."""
import os
import re
from pathlib import Path
from typing import Optional


def read_repo_files(repo_dir: str, max_files: int = 5, extensions: tuple = (".rs", ".tsx", ".ts", ".toml")) -> str:
    """Read relevant source files from repo, return as context string."""
    files_content = []
    count = 0
    
    for root, dirs, files in os.walk(repo_dir):
        # Skip common non-source directories
        dirs[:] = [d for d in dirs if d not in ('target', 'node_modules', '.git', 'dist', 'build')]
        
        for f in sorted(files):
            if count >= max_files:
                break
            if any(f.endswith(ext) for ext in extensions):
                filepath = os.path.join(root, f)
                rel_path = os.path.relpath(filepath, repo_dir)
                try:
                    content = Path(filepath).read_text()
                    if len(content) < 10000:  # Skip huge files
                        files_content.append(f"=== {rel_path} ===\n{content}")
                        count += 1
                except:
                    pass
    
    return "\n\n".join(files_content) if files_content else "(empty repo)"


def parse_llm_response(response: str) -> tuple[dict[str, str], list[str], bool]:
    """Parse LLM response into files, commands, and done status.
    
    Returns:
        (files_dict, commands_list, is_done)
    """
    files = {}
    commands = []
    done = "DONE" in response.upper()
    
    # Extract code blocks with filenames
    # Pattern: ```filename.ext\ncode\n```
    pattern = r'```(\S+\.(?:rs|ts|tsx|toml|sql|json|md|py|yaml|yml))\n(.*?)```'
    for match in re.finditer(pattern, response, re.DOTALL):
        filename = match.group(1)
        code = match.group(2).strip()
        files[filename] = code
    
    # Extract RUN commands
    for line in response.split("\n"):
        if line.strip().startswith("RUN:"):
            cmd = line.strip()[4:].strip()
            if cmd:
                commands.append(cmd)
    
    return files, commands, done


def write_code_blocks(repo_dir: str, files: dict[str, str]):
    """Write parsed code blocks to disk."""
    for filename, content in files.items():
        filepath = os.path.join(repo_dir, filename)
        os.makedirs(os.path.dirname(filepath), exist_ok=True)
        Path(filepath).write_text(content)
