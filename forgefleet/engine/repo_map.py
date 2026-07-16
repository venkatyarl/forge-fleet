"""Repo Map — dependency graph for understanding how files connect.

Extracted from Aider's repo-map concept (Apache-2.0 license).
Builds a map of: file → symbols it defines → symbols it imports.
So agents know "if I change models.rs, these 12 files import it."

Uses simple regex parsing (no LSP required, works offline).
For Rust + TypeScript + Python.
"""
import os
import re
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class Symbol:
    """A symbol (function, struct, type, etc.) defined in a file."""
    name: str
    kind: str  # "function", "struct", "trait", "type", "class", "const"
    file: str
    line: int = 0
    public: bool = False


@dataclass
class Import:
    """An import/use statement linking two files."""
    source_file: str  # File containing the import
    target: str       # What's being imported (module path or file)
    symbols: list = field(default_factory=list)  # Specific symbols imported


@dataclass
class FileInfo:
    """Information about a single file in the repo."""
    path: str
    defines: list = field(default_factory=list)  # Symbol list
    imports: list = field(default_factory=list)   # Import list
    imported_by: list = field(default_factory=list)  # Files that import this one


class RepoMap:
    """Build a dependency graph of the repository.
    
    Usage:
        rmap = RepoMap("/path/to/repo")
        rmap.build()
        
        # What does this file define?
        rmap.get_file("src/models.rs").defines
        
        # What imports this file?
        rmap.get_file("src/models.rs").imported_by
        
        # What files are affected if I change this one?
        rmap.impact_analysis("src/models.rs")
    """
    
    def __init__(self, repo_dir: str):
        self.repo_dir = repo_dir
        self.files: dict[str, FileInfo] = {}
        self.symbols: dict[str, Symbol] = {}  # name -> Symbol
    
    def build(self, extensions: tuple = (".rs", ".ts", ".tsx", ".py")) -> dict:
        """Build the full repo map."""
        exclude = {"target", "node_modules", ".git", "dist", ".next", "__pycache__", ".venv"}
        
        # Phase 1: Scan all files for definitions
        for root, dirs, filenames in os.walk(self.repo_dir):
            dirs[:] = [d for d in dirs if d not in exclude]
            for f in filenames:
                if any(f.endswith(ext) for ext in extensions):
                    filepath = os.path.join(root, f)
                    rel_path = os.path.relpath(filepath, self.repo_dir)
                    
                    try:
                        content = Path(filepath).read_text()
                        file_info = FileInfo(path=rel_path)
                        
                        ext = os.path.splitext(f)[1]
                        if ext == ".rs":
                            file_info.defines = self._parse_rust_definitions(rel_path, content)
                            file_info.imports = self._parse_rust_imports(rel_path, content)
                        elif ext in (".ts", ".tsx"):
                            file_info.defines = self._parse_ts_definitions(rel_path, content)
                            file_info.imports = self._parse_ts_imports(rel_path, content)
                        elif ext == ".py":
                            file_info.defines = self._parse_python_definitions(rel_path, content)
                            file_info.imports = self._parse_python_imports(rel_path, content)
                        
                        self.files[rel_path] = file_info
                        
                        for sym in file_info.defines:
                            self.symbols[sym.name] = sym
                    except Exception:
                        pass
        
        # Phase 2: Resolve imports → build imported_by links
        for path, info in self.files.items():
            for imp in info.imports:
                # Try to find the target file
                target_file = self._resolve_import(imp.target, path)
                if target_file and target_file in self.files:
                    if path not in self.files[target_file].imported_by:
                        self.files[target_file].imported_by.append(path)
        
        return {
            "files": len(self.files),
            "symbols": len(self.symbols),
            "imports": sum(len(f.imports) for f in self.files.values()),
        }
    
    def get_file(self, path: str) -> FileInfo:
        """Get info about a specific file."""
        return self.files.get(path, FileInfo(path=path))
    
    def impact_analysis(self, filepath: str) -> list[str]:
        """Get all files that would be affected by changing this file.
        
        Walks the imported_by graph recursively.
        """
        affected = set()
        queue = [filepath]
        
        while queue:
            current = queue.pop(0)
            if current in affected:
                continue
            affected.add(current)
            
            info = self.files.get(current)
            if info:
                for importer in info.imported_by:
                    if importer not in affected:
                        queue.append(importer)
        
        affected.discard(filepath)  # Don't include the file itself
        return sorted(affected)
    
    def context_for_task(self, task_description: str, max_files: int = 10) -> str:
        """Get relevant files for a task based on keyword matching + dependency graph.
        
        Returns a formatted context string showing relevant files and their connections.
        """
        keywords = set(task_description.lower().split())
        
        # Score files by keyword relevance
        scored = []
        for path, info in self.files.items():
            score = 0
            path_lower = path.lower()
            
            for kw in keywords:
                if kw in path_lower:
                    score += 3
                for sym in info.defines:
                    if kw in sym.name.lower():
                        score += 2
            
            if score > 0:
                scored.append((score, path, info))
        
        scored.sort(reverse=True)
        top_files = scored[:max_files]
        
        # Build context
        lines = [f"## Relevant files for: {task_description[:60]}\n"]
        
        for score, path, info in top_files:
            lines.append(f"### {path}")
            
            if info.defines:
                pub_syms = [s for s in info.defines if s.public]
                if pub_syms:
                    lines.append(f"  Exports: {', '.join(s.name for s in pub_syms[:10])}")
            
            if info.imports:
                lines.append(f"  Imports from: {', '.join(i.target for i in info.imports[:5])}")
            
            if info.imported_by:
                lines.append(f"  Used by: {', '.join(info.imported_by[:5])}")
            
            lines.append("")
        
        return "\n".join(lines)
    
    def summary(self) -> str:
        """Get a human-readable summary of the repo structure."""
        lines = [f"Repo Map: {len(self.files)} files, {len(self.symbols)} symbols\n"]
        
        # Group by directory
        by_dir = {}
        for path in sorted(self.files.keys()):
            dirname = os.path.dirname(path)
            by_dir.setdefault(dirname, []).append(path)
        
        for dirname, files in sorted(by_dir.items()):
            lines.append(f"  {dirname or '.'}/ ({len(files)} files)")
            for f in files[:5]:
                info = self.files[f]
                syms = len(info.defines)
                imports = len(info.imports)
                used_by = len(info.imported_by)
                lines.append(f"    {os.path.basename(f)}: {syms} symbols, {imports} imports, used by {used_by}")
            if len(files) > 5:
                lines.append(f"    ... and {len(files) - 5} more")
        
        return "\n".join(lines)
    
    # ─── Language-specific parsers ──────────────────
    
    def _parse_rust_definitions(self, filepath: str, content: str) -> list[Symbol]:
        """Extract Rust definitions (pub fn, struct, enum, trait, const)."""
        symbols = []
        for i, line in enumerate(content.split("\n"), 1):
            stripped = line.strip()
            
            # pub fn name(
            m = re.match(r'pub\s+(?:async\s+)?fn\s+(\w+)', stripped)
            if m:
                symbols.append(Symbol(m.group(1), "function", filepath, i, True))
                continue
            
            # fn name( (private)
            m = re.match(r'fn\s+(\w+)', stripped)
            if m and not stripped.startswith("pub"):
                symbols.append(Symbol(m.group(1), "function", filepath, i, False))
                continue
            
            # pub struct Name
            m = re.match(r'pub\s+struct\s+(\w+)', stripped)
            if m:
                symbols.append(Symbol(m.group(1), "struct", filepath, i, True))
                continue
            
            # pub enum Name
            m = re.match(r'pub\s+enum\s+(\w+)', stripped)
            if m:
                symbols.append(Symbol(m.group(1), "enum", filepath, i, True))
                continue
            
            # pub trait Name
            m = re.match(r'pub\s+trait\s+(\w+)', stripped)
            if m:
                symbols.append(Symbol(m.group(1), "trait", filepath, i, True))
                continue
            
            # pub type Name
            m = re.match(r'pub\s+type\s+(\w+)', stripped)
            if m:
                symbols.append(Symbol(m.group(1), "type", filepath, i, True))
                continue
            
            # pub const NAME
            m = re.match(r'pub\s+const\s+(\w+)', stripped)
            if m:
                symbols.append(Symbol(m.group(1), "const", filepath, i, True))
                continue
        
        return symbols
    
    def _parse_rust_imports(self, filepath: str, content: str) -> list[Import]:
        """Extract Rust use/mod statements."""
        imports = []
        for line in content.split("\n"):
            stripped = line.strip()
            
            # use crate::module::Symbol;
            m = re.match(r'use\s+(crate|super|self)::(\S+?);', stripped)
            if m:
                imports.append(Import(filepath, m.group(2).split("::")[0]))
                continue
            
            # use external_crate::...
            m = re.match(r'use\s+(\w+)::', stripped)
            if m and m.group(1) not in ("crate", "super", "self", "std", "core", "alloc"):
                imports.append(Import(filepath, m.group(1)))
                continue
            
            # mod module_name;
            m = re.match(r'(?:pub\s+)?mod\s+(\w+);', stripped)
            if m:
                imports.append(Import(filepath, m.group(1)))
        
        return imports
    
    def _parse_ts_definitions(self, filepath: str, content: str) -> list[Symbol]:
        """Extract TypeScript definitions."""
        symbols = []
        for i, line in enumerate(content.split("\n"), 1):
            stripped = line.strip()
            
            # export function name(
            m = re.match(r'export\s+(?:async\s+)?function\s+(\w+)', stripped)
            if m:
                symbols.append(Symbol(m.group(1), "function", filepath, i, True))
                continue
            
            # export interface/type Name
            m = re.match(r'export\s+(?:interface|type)\s+(\w+)', stripped)
            if m:
                symbols.append(Symbol(m.group(1), "type", filepath, i, True))
                continue
            
            # export class Name
            m = re.match(r'export\s+(?:default\s+)?class\s+(\w+)', stripped)
            if m:
                symbols.append(Symbol(m.group(1), "class", filepath, i, True))
                continue
            
            # export const Name
            m = re.match(r'export\s+const\s+(\w+)', stripped)
            if m:
                symbols.append(Symbol(m.group(1), "const", filepath, i, True))
        
        return symbols
    
    def _parse_ts_imports(self, filepath: str, content: str) -> list[Import]:
        """Extract TypeScript imports."""
        imports = []
        for line in content.split("\n"):
            # import { X } from './module'
            m = re.match(r"import\s+.*from\s+['\"]([^'\"]+)['\"]", line.strip())
            if m:
                target = m.group(1)
                if target.startswith("."):
                    imports.append(Import(filepath, target))
        
        return imports
    
    def _parse_python_definitions(self, filepath: str, content: str) -> list[Symbol]:
        """Extract Python definitions."""
        symbols = []
        for i, line in enumerate(content.split("\n"), 1):
            m = re.match(r'(?:async\s+)?def\s+(\w+)', line.strip())
            if m and not m.group(1).startswith("_"):
                symbols.append(Symbol(m.group(1), "function", filepath, i, True))
            
            m = re.match(r'class\s+(\w+)', line.strip())
            if m:
                symbols.append(Symbol(m.group(1), "class", filepath, i, True))
        
        return symbols
    
    def _parse_python_imports(self, filepath: str, content: str) -> list[Import]:
        """Extract Python imports."""
        imports = []
        for line in content.split("\n"):
            m = re.match(r'from\s+(\S+)\s+import', line.strip())
            if m and not m.group(1).startswith(("os", "sys", "json", "re", "time", "typing")):
                imports.append(Import(filepath, m.group(1)))
            
            m = re.match(r'import\s+(\S+)', line.strip())
            if m and not m.group(1).startswith(("os", "sys", "json", "re", "time", "typing")):
                imports.append(Import(filepath, m.group(1)))
        
        return imports
    
    def _resolve_import(self, target: str, from_file: str) -> str:
        """Try to resolve an import target to a file path."""
        # For relative imports (./module)
        if target.startswith("."):
            from_dir = os.path.dirname(from_file)
            # Try .ts, .tsx, .py, /index.ts
            for suffix in (".ts", ".tsx", ".py", "/index.ts", "/index.tsx", "/mod.rs"):
                candidate = os.path.normpath(os.path.join(from_dir, target + suffix))
                if candidate in self.files:
                    return candidate
        
        # For Rust module imports
        from_dir = os.path.dirname(from_file)
        for suffix in (".rs", "/mod.rs", "/lib.rs"):
            candidate = os.path.join(from_dir, target + suffix)
            if candidate in self.files:
                return candidate
        
        return ""
