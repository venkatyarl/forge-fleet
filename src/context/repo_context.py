"""Repository context builder — feeds relevant code to LLMs efficiently."""
import os
import subprocess
import json
from pathlib import Path
from typing import Optional


class RepoContext:
    """Builds smart context from a repository for LLM prompts.
    
    Uses multiple strategies:
    1. Aider repo-map (if available) — AST-based codebase understanding
    2. Tree-sitter parsing — function signatures and structure
    3. Simple file reading — fallback
    
    Optimizes for token efficiency — only includes relevant code.
    """
    
    def __init__(self, repo_dir: str, max_tokens: int = 8000):
        self.repo_dir = repo_dir
        self.max_tokens = max_tokens
        self._aider_available = self._check_aider()
        self._cocoindex_available = self._check_cocoindex()
    
    def _check_aider(self) -> bool:
        try:
            import aider.repomap
            return True
        except ImportError:
            return False
    
    def _check_cocoindex(self) -> bool:
        try:
            import cocoindex_code
            return True
        except ImportError:
            return False
    
    def get_context(self, task_description: str, specific_files: list[str] = None) -> str:
        """Get relevant code context for a task.
        
        Tries in order:
        1. CocoIndex AST search (most token-efficient)
        2. Aider repo-map (understands full structure)
        3. Simple file reading (fallback)
        """
        if specific_files:
            return self._read_specific_files(specific_files)
        
        if self._cocoindex_available:
            ctx = self._cocoindex_search(task_description)
            if ctx:
                return ctx
        
        if self._aider_available:
            ctx = self._aider_repomap(task_description)
            if ctx:
                return ctx
        
        return self._simple_context(task_description)
    
    def _cocoindex_search(self, query: str) -> Optional[str]:
        """Use CocoIndex for AST-based code search — saves 70% tokens."""
        try:
            result = subprocess.run(
                ["cocoindex-code", "search", query, "--repo", self.repo_dir, "--limit", "5"],
                capture_output=True, text=True, timeout=30
            )
            if result.returncode == 0 and result.stdout.strip():
                return f"## Relevant code (AST search):\n{result.stdout[:self.max_tokens * 4]}"
        except:
            pass
        return None
    
    def _aider_repomap(self, query: str) -> Optional[str]:
        """Use Aider's repo-map for structural understanding."""
        try:
            from aider.repomap import RepoMap
            from aider.io import InputOutput
            
            io = InputOutput(yes=True)
            rm = RepoMap(root=self.repo_dir, io=io)
            
            # Get all tracked files
            all_files = []
            for root, dirs, files in os.walk(self.repo_dir):
                dirs[:] = [d for d in dirs if d not in ('target', 'node_modules', '.git')]
                for f in files:
                    if f.endswith(('.rs', '.tsx', '.ts', '.toml')):
                        all_files.append(os.path.join(root, f))
            
            if all_files:
                repo_map = rm.get_repo_map([], all_files[:50])
                if repo_map:
                    return f"## Repository structure:\n{repo_map[:self.max_tokens * 4]}"
        except Exception as e:
            pass
        return None
    
    def _simple_context(self, query: str) -> str:
        """Simple file reading — read relevant files based on keywords."""
        keywords = query.lower().split()
        relevant_files = []
        
        for root, dirs, files in os.walk(self.repo_dir):
            dirs[:] = [d for d in dirs if d not in ('target', 'node_modules', '.git', 'dist')]
            for f in files:
                if not f.endswith(('.rs', '.tsx', '.ts', '.toml')):
                    continue
                filepath = os.path.join(root, f)
                rel_path = os.path.relpath(filepath, self.repo_dir).lower()
                # Score by keyword match in path
                score = sum(1 for kw in keywords if kw in rel_path)
                if score > 0:
                    relevant_files.append((score, filepath, rel_path))
        
        # Sort by relevance, take top files
        relevant_files.sort(key=lambda x: -x[0])
        
        context_parts = []
        total_chars = 0
        max_chars = self.max_tokens * 4  # rough token-to-char ratio
        
        for score, filepath, rel_path in relevant_files[:10]:
            try:
                content = Path(filepath).read_text()
                if total_chars + len(content) > max_chars:
                    break
                context_parts.append(f"=== {rel_path} ===\n{content}")
                total_chars += len(content)
            except:
                pass
        
        if not context_parts:
            # No keyword matches — read Cargo.toml and lib.rs files
            for root, dirs, files in os.walk(self.repo_dir):
                dirs[:] = [d for d in dirs if d not in ('target', 'node_modules', '.git')]
                for f in files:
                    if f in ('Cargo.toml', 'lib.rs', 'main.rs', 'package.json'):
                        filepath = os.path.join(root, f)
                        rel_path = os.path.relpath(filepath, self.repo_dir)
                        try:
                            content = Path(filepath).read_text()
                            if total_chars + len(content) > max_chars:
                                break
                            context_parts.append(f"=== {rel_path} ===\n{content}")
                            total_chars += len(content)
                        except:
                            pass
        
        return "\n\n".join(context_parts) if context_parts else "(empty repo)"
    
    def _read_specific_files(self, files: list[str]) -> str:
        """Read specific files for context."""
        parts = []
        for f in files:
            filepath = os.path.join(self.repo_dir, f)
            try:
                content = Path(filepath).read_text()
                parts.append(f"=== {f} ===\n{content}")
            except:
                parts.append(f"=== {f} === (not found)")
        return "\n\n".join(parts)
    
    def get_architecture(self) -> str:
        """Get ARCHITECTURE_PRINCIPLES.md if it exists."""
        arch_path = os.path.join(self.repo_dir, "ARCHITECTURE_PRINCIPLES.md")
        if os.path.exists(arch_path):
            return Path(arch_path).read_text()[:4000]
        return ""
