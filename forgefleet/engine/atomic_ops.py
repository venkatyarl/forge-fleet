"""Atomic Operations — all-or-nothing file changes with rollback.

Item #6: If writing file 3 of 5 fails, rollback files 1 and 2.
Like a database transaction but for the filesystem.
"""
import os
import shutil
import tempfile
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class FileChange:
    """A pending file change."""
    filepath: str
    new_content: str
    action: str  # "create", "modify", "delete"
    original_content: str = ""  # For rollback
    original_exists: bool = False


class AtomicTransaction:
    """Apply multiple file changes as one atomic operation.
    
    Usage:
        tx = AtomicTransaction(repo_dir)
        tx.write("src/models.rs", new_content)
        tx.write("src/handlers.rs", new_content)
        tx.delete("src/old_file.rs")
        
        if tx.commit():
            print("All changes applied")
        else:
            print("Failed — all changes rolled back")
    """
    
    def __init__(self, repo_dir: str):
        self.repo_dir = repo_dir
        self.changes: list[FileChange] = []
        self._committed = False
        self._backup_dir = tempfile.mkdtemp(prefix="forgefleet-tx-")
    
    def write(self, filepath: str, content: str):
        """Stage a file write."""
        full_path = os.path.join(self.repo_dir, filepath)
        
        change = FileChange(
            filepath=filepath,
            new_content=content,
            action="modify" if os.path.exists(full_path) else "create",
            original_exists=os.path.exists(full_path),
        )
        
        # Save original for rollback
        if change.original_exists:
            change.original_content = Path(full_path).read_text()
        
        self.changes.append(change)
    
    def delete(self, filepath: str):
        """Stage a file deletion."""
        full_path = os.path.join(self.repo_dir, filepath)
        
        change = FileChange(
            filepath=filepath,
            new_content="",
            action="delete",
            original_exists=os.path.exists(full_path),
        )
        
        if change.original_exists:
            change.original_content = Path(full_path).read_text()
        
        self.changes.append(change)
    
    def commit(self) -> bool:
        """Apply all changes. Rollback on any failure."""
        applied = []
        
        try:
            for change in self.changes:
                full_path = os.path.join(self.repo_dir, change.filepath)
                
                # Backup original
                if change.original_exists:
                    backup_path = os.path.join(self._backup_dir, change.filepath)
                    os.makedirs(os.path.dirname(backup_path), exist_ok=True)
                    shutil.copy2(full_path, backup_path)
                
                # Apply change
                if change.action in ("create", "modify"):
                    os.makedirs(os.path.dirname(full_path), exist_ok=True)
                    Path(full_path).write_text(change.new_content)
                elif change.action == "delete":
                    if os.path.exists(full_path):
                        os.remove(full_path)
                
                applied.append(change)
            
            self._committed = True
            self._cleanup()
            return True
            
        except Exception as e:
            # Rollback all applied changes
            self._rollback(applied)
            self._cleanup()
            return False
    
    def _rollback(self, applied: list[FileChange]):
        """Rollback applied changes to original state."""
        for change in reversed(applied):
            full_path = os.path.join(self.repo_dir, change.filepath)
            
            try:
                if change.action == "create":
                    # Was created — delete it
                    if os.path.exists(full_path):
                        os.remove(full_path)
                elif change.action == "modify":
                    # Was modified — restore original
                    Path(full_path).write_text(change.original_content)
                elif change.action == "delete":
                    # Was deleted — restore from backup
                    backup_path = os.path.join(self._backup_dir, change.filepath)
                    if os.path.exists(backup_path):
                        shutil.copy2(backup_path, full_path)
            except Exception:
                pass  # Best effort rollback
    
    def _cleanup(self):
        """Clean up backup directory."""
        try:
            shutil.rmtree(self._backup_dir, ignore_errors=True)
        except Exception:
            pass
    
    def preview(self) -> str:
        """Preview pending changes without applying."""
        lines = [f"Transaction: {len(self.changes)} changes"]
        for c in self.changes:
            icon = {"create": "➕", "modify": "✏️", "delete": "🗑️"}.get(c.action, "?")
            size = len(c.new_content) if c.action != "delete" else 0
            lines.append(f"  {icon} {c.action}: {c.filepath} ({size} chars)")
        return "\n".join(lines)
