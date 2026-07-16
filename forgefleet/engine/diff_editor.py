"""Diff Editor — parse and apply unified diffs to files.

Extracted from Aider's diff patterns (Apache-2.0 license).
Handles the common ways LLMs output code changes:
1. Unified diff format (--- a/file, +++ b/file, @@ hunks)
2. Search/Replace blocks (<<<< SEARCH / ==== / REPLACE >>>>)
3. Full file replacement (when diff is the complete file)
"""
import os
import re
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class EditResult:
    """Result of applying an edit."""
    filepath: str
    success: bool
    action: str  # "created", "modified", "deleted"
    lines_added: int = 0
    lines_removed: int = 0
    error: str = ""


class DiffEditor:
    """Parse LLM output and apply file changes.
    
    Supports multiple edit formats that LLMs commonly produce:
    - Unified diff (git diff format)
    - Search/Replace blocks (Aider's preferred format)
    - Full file content with filepath header
    """
    
    def __init__(self, repo_dir: str):
        self.repo_dir = repo_dir
    
    def apply_llm_output(self, output: str) -> list[EditResult]:
        """Parse LLM output and apply all edits found.
        
        Tries formats in order:
        1. Search/Replace blocks
        2. Unified diff hunks
        3. Full file blocks (```filepath ... ```)
        """
        results = []
        
        # Try search/replace blocks first
        sr_edits = self._parse_search_replace(output)
        if sr_edits:
            for filepath, old_text, new_text in sr_edits:
                result = self._apply_search_replace(filepath, old_text, new_text)
                results.append(result)
            return results
        
        # Try unified diff
        diff_edits = self._parse_unified_diff(output)
        if diff_edits:
            for filepath, hunks in diff_edits:
                result = self._apply_diff_hunks(filepath, hunks)
                results.append(result)
            return results
        
        # Try full file blocks
        file_blocks = self._parse_file_blocks(output)
        if file_blocks:
            for filepath, content in file_blocks:
                result = self._write_file(filepath, content)
                results.append(result)
            return results
        
        return results
    
    def _parse_search_replace(self, text: str) -> list[tuple]:
        """Parse Aider-style search/replace blocks.
        
        Format:
        filepath
        <<<<<<< SEARCH
        old code
        =======
        new code
        >>>>>>> REPLACE
        """
        pattern = r'([^\n]+)\n<<<<<<< SEARCH\n(.*?)\n=======\n(.*?)\n>>>>>>> REPLACE'
        matches = re.findall(pattern, text, re.DOTALL)
        
        results = []
        for filepath, old_text, new_text in matches:
            filepath = filepath.strip().strip('`').strip()
            # Clean common prefixes
            for prefix in ['a/', 'b/', './']:
                if filepath.startswith(prefix):
                    filepath = filepath[len(prefix):]
            results.append((filepath, old_text, new_text))
        
        return results
    
    def _parse_unified_diff(self, text: str) -> list[tuple]:
        """Parse unified diff format.
        
        Format:
        --- a/filepath
        +++ b/filepath
        @@ -start,count +start,count @@
        -removed line
        +added line
         context line
        """
        results = []
        current_file = None
        current_hunks = []
        
        lines = text.split('\n')
        i = 0
        while i < len(lines):
            line = lines[i]
            
            # New file header
            if line.startswith('--- a/') or line.startswith('--- '):
                if current_file and current_hunks:
                    results.append((current_file, current_hunks))
                
                # Get filename from +++ line
                if i + 1 < len(lines) and lines[i + 1].startswith('+++ '):
                    filepath = lines[i + 1][4:].strip()
                    for prefix in ['b/', './']:
                        if filepath.startswith(prefix):
                            filepath = filepath[len(prefix):]
                    current_file = filepath
                    current_hunks = []
                    i += 2
                    continue
            
            # Hunk header
            if line.startswith('@@') and current_file:
                hunk_lines = []
                i += 1
                while i < len(lines):
                    l = lines[i]
                    if l.startswith('@@') or l.startswith('--- ') or l.startswith('diff '):
                        break
                    hunk_lines.append(l)
                    i += 1
                current_hunks.append(hunk_lines)
                continue
            
            i += 1
        
        if current_file and current_hunks:
            results.append((current_file, current_hunks))
        
        return results
    
    def _parse_file_blocks(self, text: str) -> list[tuple]:
        """Parse full file content blocks.
        
        Format:
        ```filepath.rs
        content
        ```
        
        Or:
        // filepath.rs
        content
        """
        results = []
        
        # Match ```filename\ncontent\n```
        pattern = r'```(\S+(?:\.\w+)+)\n(.*?)```'
        matches = re.findall(pattern, text, re.DOTALL)
        
        for filepath, content in matches:
            # Skip language-only markers like ```rust
            if '.' not in filepath or filepath in ('rust', 'python', 'typescript', 'javascript', 'toml', 'json', 'yaml', 'bash', 'shell', 'sql'):
                continue
            results.append((filepath.strip(), content.strip()))
        
        return results
    
    def _apply_search_replace(self, filepath: str, old_text: str, new_text: str) -> EditResult:
        """Apply a search/replace edit to a file."""
        full_path = os.path.join(self.repo_dir, filepath)
        
        if not os.path.exists(full_path):
            # If old_text is empty, this is a new file
            if not old_text.strip():
                return self._write_file(filepath, new_text)
            return EditResult(filepath=filepath, success=False, action="error",
                            error=f"File not found: {filepath}")
        
        try:
            content = Path(full_path).read_text()
            
            if old_text not in content:
                # Try with normalized whitespace
                normalized_old = re.sub(r'\s+', ' ', old_text.strip())
                normalized_content = re.sub(r'\s+', ' ', content)
                if normalized_old not in normalized_content:
                    return EditResult(filepath=filepath, success=False, action="error",
                                    error="Search text not found in file")
                # Fuzzy match — find the actual text and replace
                # For now, just report the error
                return EditResult(filepath=filepath, success=False, action="error",
                                error="Search text not found (exact match required)")
            
            new_content = content.replace(old_text, new_text, 1)
            Path(full_path).write_text(new_content)
            
            added = len(new_text.split('\n'))
            removed = len(old_text.split('\n'))
            
            return EditResult(
                filepath=filepath, success=True, action="modified",
                lines_added=added, lines_removed=removed,
            )
        except Exception as e:
            return EditResult(filepath=filepath, success=False, action="error", error=str(e))
    
    def _apply_diff_hunks(self, filepath: str, hunks: list) -> EditResult:
        """Apply unified diff hunks to a file."""
        full_path = os.path.join(self.repo_dir, filepath)
        
        if not os.path.exists(full_path):
            # New file — collect all added lines
            added_lines = []
            for hunk in hunks:
                for line in hunk:
                    if line.startswith('+'):
                        added_lines.append(line[1:])
                    elif not line.startswith('-'):
                        added_lines.append(line[1:] if line.startswith(' ') else line)
            return self._write_file(filepath, '\n'.join(added_lines))
        
        try:
            content = Path(full_path).read_text()
            lines = content.split('\n')
            total_added = 0
            total_removed = 0
            
            for hunk in hunks:
                # Build the expected old content and new content
                old_lines = []
                new_lines = []
                for line in hunk:
                    if line.startswith('-'):
                        old_lines.append(line[1:])
                        total_removed += 1
                    elif line.startswith('+'):
                        new_lines.append(line[1:])
                        total_added += 1
                    elif line.startswith(' '):
                        old_lines.append(line[1:])
                        new_lines.append(line[1:])
                    else:
                        old_lines.append(line)
                        new_lines.append(line)
                
                # Find and replace the old block
                old_block = '\n'.join(old_lines)
                new_block = '\n'.join(new_lines)
                
                if old_block in content:
                    content = content.replace(old_block, new_block, 1)
            
            Path(full_path).write_text(content)
            
            return EditResult(
                filepath=filepath, success=True, action="modified",
                lines_added=total_added, lines_removed=total_removed,
            )
        except Exception as e:
            return EditResult(filepath=filepath, success=False, action="error", error=str(e))
    
    def _write_file(self, filepath: str, content: str) -> EditResult:
        """Write a complete file."""
        full_path = os.path.join(self.repo_dir, filepath)
        
        try:
            os.makedirs(os.path.dirname(full_path), exist_ok=True)
            is_new = not os.path.exists(full_path)
            Path(full_path).write_text(content)
            
            return EditResult(
                filepath=filepath, success=True,
                action="created" if is_new else "modified",
                lines_added=len(content.split('\n')),
            )
        except Exception as e:
            return EditResult(filepath=filepath, success=False, action="error", error=str(e))
