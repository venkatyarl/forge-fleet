"""Memory Sync — share learnings across nodes via SQLite.

Item #10: Agents on different nodes share what they've learned.
When Node A discovers a fix for a Rust error, Node B knows about it.

Uses simple file-based sync: export → SCP → import.
"""
import json
import os
import subprocess
import time
import sqlite3
from dataclasses import dataclass, field


@dataclass
class MemorySync:
    """Sync ForgeFleet's SQLite databases across fleet nodes.
    
    Databases to sync:
    - learnings.db (self_improve)
    - context.db (context_store)  
    - upstream_state.json (upstream_monitor)
    
    Strategy: master node (Taylor) is authoritative.
    Other nodes export new records → Taylor merges → Taylor pushes back.
    """
    local_db_dir: str = ""
    fleet_nodes: list = field(default_factory=lambda: [
        "james", "marcus", "sophie", "priya", "ace"
    ])
    
    def __post_init__(self):
        if not self.local_db_dir:
            self.local_db_dir = os.path.expanduser("~/.forgefleet")
    
    def export_learnings(self) -> str:
        """Export recent learnings as JSON for syncing."""
        db_path = os.path.join(self.local_db_dir, "learnings.db")
        if not os.path.exists(db_path):
            return "[]"
        
        db = sqlite3.connect(db_path)
        # Get learnings from last 24h
        cutoff = time.time() - 86400
        rows = db.execute(
            "SELECT task_type, model_used, tier, outcome, error_pattern, fix_applied, duration_seconds, timestamp FROM learnings WHERE timestamp > ?",
            (cutoff,)
        ).fetchall()
        db.close()
        
        return json.dumps([
            {"task_type": r[0], "model_used": r[1], "tier": r[2], "outcome": r[3],
             "error_pattern": r[4], "fix_applied": r[5], "duration": r[6], "ts": r[7]}
            for r in rows
        ])
    
    def import_learnings(self, data_json: str):
        """Import learnings from another node."""
        db_path = os.path.join(self.local_db_dir, "learnings.db")
        db = sqlite3.connect(db_path)
        
        records = json.loads(data_json)
        for r in records:
            # Check for duplicates
            existing = db.execute(
                "SELECT id FROM learnings WHERE task_type=? AND model_used=? AND timestamp=?",
                (r["task_type"], r["model_used"], r["ts"])
            ).fetchone()
            
            if not existing:
                db.execute(
                    "INSERT INTO learnings (task_type, model_used, tier, outcome, error_pattern, fix_applied, duration_seconds, timestamp) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    (r["task_type"], r["model_used"], r["tier"], r["outcome"],
                     r.get("error_pattern", ""), r.get("fix_applied", ""),
                     r.get("duration", 0), r["ts"]),
                )
        
        db.commit()
        db.close()
    
    def sync_to_nodes(self):
        """Push local learnings to all fleet nodes."""
        data = self.export_learnings()
        if data == "[]":
            return {"synced": 0, "reason": "No new learnings"}
        
        results = {}
        for node in self.fleet_nodes:
            try:
                # Write to temp file and SCP
                tmp = f"/tmp/forgefleet-sync-{os.getpid()}.json"
                with open(tmp, "w") as f:
                    f.write(data)
                
                subprocess.run(
                    ["scp", "-q", tmp, f"{node}:~/.forgefleet/sync_import.json"],
                    capture_output=True, timeout=10,
                )
                
                # Run import on remote node
                subprocess.run(
                    ["ssh", node, "cd ~/.forgefleet && python3 -c \"from forgefleet.engine.memory_sync import MemorySync; m=MemorySync(); m.import_learnings(open('sync_import.json').read())\" 2>/dev/null"],
                    capture_output=True, timeout=15,
                )
                
                results[node] = "synced"
                os.remove(tmp)
            except Exception as e:
                results[node] = f"failed: {e}"
        
        return results
    
    def pull_from_nodes(self):
        """Pull learnings from all fleet nodes to Taylor."""
        results = {}
        for node in self.fleet_nodes:
            try:
                r = subprocess.run(
                    ["ssh", node, "python3 -c \"from forgefleet.engine.memory_sync import MemorySync; m=MemorySync(); print(m.export_learnings())\" 2>/dev/null"],
                    capture_output=True, text=True, timeout=15,
                )
                if r.stdout.strip() and r.stdout.strip() != "[]":
                    self.import_learnings(r.stdout.strip())
                    results[node] = f"imported {len(json.loads(r.stdout.strip()))} records"
                else:
                    results[node] = "no new data"
            except Exception as e:
                results[node] = f"failed: {e}"
        
        return results
