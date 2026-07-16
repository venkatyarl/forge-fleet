"""Evolution Engine — ForgeFleet learns from every task and improves itself.

After every task, records:
- What worked (tool calls that produced results)
- What failed (iterations wasted, tools not called, empty outputs)
- Time per phase (reading vs writing vs reviewing)
- Model performance (which model was best for what)

Periodically analyzes all data and generates:
- Improvement proposals for the next version
- Prompt template adjustments
- Model routing recommendations
- Workflow optimizations
"""
import json
import os
import sqlite3
import time
from dataclasses import dataclass, field
from datetime import datetime


@dataclass
class TaskRecord:
    """Complete record of a task execution for learning."""
    task_id: str
    title: str
    task_type: str  # "rust_handler", "typescript_page", "schema", etc.
    
    # Execution details
    total_time: float = 0
    reader_time: float = 0
    writer_time: float = 0
    reviewer_time: float = 0
    
    # Tool usage
    read_calls: int = 0
    write_calls: int = 0
    list_calls: int = 0
    cmd_calls: int = 0
    total_iterations: int = 0
    wasted_iterations: int = 0  # Iterations that produced no useful output
    
    # Models used
    reader_model: str = ""
    writer_model: str = ""
    reviewer_model: str = ""
    
    # Output quality
    files_created: int = 0
    lines_written: int = 0
    had_junk_files: bool = False  # Files created that shouldn't have been
    needed_manual_fix: bool = False
    review_verdict: str = ""  # "pass", "needs_work", "fail"
    
    # Outcome
    success: bool = False
    pushed: bool = False
    error: str = ""
    
    timestamp: float = 0


@dataclass
class Insight:
    """An insight derived from analyzing task records."""
    category: str  # "prompt", "model", "workflow", "tool", "quality"
    finding: str
    recommendation: str
    confidence: float  # 0-1
    evidence_count: int  # How many tasks support this


class EvolutionEngine:
    """ForgeFleet's self-improvement system.
    
    Records everything, analyzes patterns, generates improvement proposals.
    """
    
    def __init__(self, db_path: str = ""):
        if not db_path:
            db_dir = os.path.expanduser("~/.forgefleet")
            os.makedirs(db_dir, exist_ok=True)
            db_path = os.path.join(db_dir, "evolution.db")
        
        self.db = sqlite3.connect(db_path)
        self.db.execute("PRAGMA journal_mode=WAL")
        self._init_schema()
    
    def _init_schema(self):
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS task_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT,
                title TEXT,
                task_type TEXT,
                total_time REAL,
                reader_time REAL,
                writer_time REAL,
                reviewer_time REAL,
                read_calls INTEGER DEFAULT 0,
                write_calls INTEGER DEFAULT 0,
                list_calls INTEGER DEFAULT 0,
                cmd_calls INTEGER DEFAULT 0,
                total_iterations INTEGER DEFAULT 0,
                wasted_iterations INTEGER DEFAULT 0,
                reader_model TEXT,
                writer_model TEXT,
                reviewer_model TEXT,
                files_created INTEGER DEFAULT 0,
                lines_written INTEGER DEFAULT 0,
                had_junk_files INTEGER DEFAULT 0,
                needed_manual_fix INTEGER DEFAULT 0,
                review_verdict TEXT,
                success INTEGER DEFAULT 0,
                pushed INTEGER DEFAULT 0,
                error TEXT DEFAULT '',
                timestamp REAL
            );
            
            CREATE TABLE IF NOT EXISTS insights (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                category TEXT,
                finding TEXT,
                recommendation TEXT,
                confidence REAL,
                evidence_count INTEGER,
                created_at TEXT DEFAULT (datetime('now')),
                applied INTEGER DEFAULT 0
            );
            
            CREATE TABLE IF NOT EXISTS version_proposals (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                version TEXT,
                title TEXT,
                description TEXT,
                priority TEXT,
                status TEXT DEFAULT 'proposed',
                created_at TEXT DEFAULT (datetime('now'))
            );
        """)
    
    def record_task(self, record: TaskRecord):
        """Record a completed task for analysis."""
        if not record.timestamp:
            record.timestamp = time.time()
        
        self.db.execute("""
            INSERT INTO task_records 
            (task_id, title, task_type, total_time, reader_time, writer_time, reviewer_time,
             read_calls, write_calls, list_calls, cmd_calls, total_iterations, wasted_iterations,
             reader_model, writer_model, reviewer_model, files_created, lines_written,
             had_junk_files, needed_manual_fix, review_verdict, success, pushed, error, timestamp)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
        """, (record.task_id, record.title, record.task_type, record.total_time,
              record.reader_time, record.writer_time, record.reviewer_time,
              record.read_calls, record.write_calls, record.list_calls, record.cmd_calls,
              record.total_iterations, record.wasted_iterations,
              record.reader_model, record.writer_model, record.reviewer_model,
              record.files_created, record.lines_written,
              int(record.had_junk_files), int(record.needed_manual_fix),
              record.review_verdict, int(record.success), int(record.pushed),
              record.error, record.timestamp))
        self.db.commit()
    
    def analyze(self) -> list[Insight]:
        """Analyze all task records and generate insights."""
        insights = []
        
        records = self.db.execute("SELECT * FROM task_records ORDER BY timestamp DESC LIMIT 100").fetchall()
        if len(records) < 3:
            return [Insight("data", "Not enough data yet", "Run more tasks", 0.5, len(records))]
        
        # Analyze success rate
        total = len(records)
        successes = sum(1 for r in records if r[22])  # success column
        rate = successes / total if total else 0
        
        if rate < 0.5:
            insights.append(Insight(
                "quality", f"Low success rate: {rate:.0%} ({successes}/{total})",
                "Consider using larger models or simplifying task descriptions",
                0.9, total,
            ))
        
        # Analyze wasted iterations
        total_iterations = sum(r[11] for r in records)  # total_iterations
        wasted = sum(r[12] for r in records)  # wasted_iterations
        if total_iterations > 0 and wasted / total_iterations > 0.3:
            insights.append(Insight(
                "workflow", f"{wasted}/{total_iterations} iterations wasted ({wasted/total_iterations:.0%})",
                "Improve prompt clarity so models use tools instead of describing",
                0.85, total,
            ))
        
        # Analyze tool usage
        write_calls = sum(r[8] for r in records)  # write_calls
        tasks_with_writes = sum(1 for r in records if r[8] > 0)
        if tasks_with_writes < total * 0.7:
            insights.append(Insight(
                "tool", f"Only {tasks_with_writes}/{total} tasks used write_file",
                "Model often describes code instead of writing files. Strengthen tool-use instructions in prompts.",
                0.9, total,
            ))
        
        # Analyze junk files
        junk_count = sum(1 for r in records if r[18])  # had_junk_files
        if junk_count > total * 0.2:
            insights.append(Insight(
                "quality", f"{junk_count}/{total} tasks created junk files",
                "Add output validation to reject files outside the expected directory structure",
                0.8, junk_count,
            ))
        
        # Analyze manual fixes needed
        manual_fixes = sum(1 for r in records if r[19])  # needed_manual_fix
        if manual_fixes > total * 0.3:
            insights.append(Insight(
                "quality", f"{manual_fixes}/{total} tasks needed manual fixes after ForgeFleet",
                "Add stricter post-build validation: check derives, doc comments, serde attributes",
                0.85, manual_fixes,
            ))
        
        # Analyze time distribution
        avg_reader = sum(r[4] for r in records) / max(total, 1)
        avg_writer = sum(r[5] for r in records) / max(total, 1)
        avg_reviewer = sum(r[6] for r in records) / max(total, 1)
        
        if avg_writer > 120:
            insights.append(Insight(
                "workflow", f"Writer phase averages {avg_writer:.0f}s — too slow",
                "Use task decomposition to give writer smaller, focused tasks",
                0.7, total,
            ))
        
        # Analyze model performance
        model_success = {}
        for r in records:
            model = r[14]  # writer_model
            if model:
                if model not in model_success:
                    model_success[model] = {"total": 0, "success": 0}
                model_success[model]["total"] += 1
                if r[22]:  # success
                    model_success[model]["success"] += 1
        
        for model, stats in model_success.items():
            rate = stats["success"] / max(stats["total"], 1)
            if rate < 0.5 and stats["total"] >= 3:
                insights.append(Insight(
                    "model", f"{model}: {rate:.0%} success rate ({stats['success']}/{stats['total']})",
                    f"Consider replacing {model} with a better model for this role",
                    0.75, stats["total"],
                ))
        
        # Save insights
        for insight in insights:
            self.db.execute(
                "INSERT INTO insights (category, finding, recommendation, confidence, evidence_count) VALUES (?,?,?,?,?)",
                (insight.category, insight.finding, insight.recommendation, insight.confidence, insight.evidence_count),
            )
        self.db.commit()
        
        return insights
    
    def generate_version_proposal(self) -> dict:
        """Generate a proposal for the next version of ForgeFleet based on learnings."""
        insights = self.analyze()
        
        if not insights:
            return {"version": "current", "changes": [], "reason": "No actionable insights yet"}
        
        proposals = []
        for insight in sorted(insights, key=lambda i: -i.confidence):
            proposals.append({
                "category": insight.category,
                "problem": insight.finding,
                "fix": insight.recommendation,
                "priority": "high" if insight.confidence > 0.8 else "medium",
                "evidence": insight.evidence_count,
            })
        
        version = {
            "version": f"v{datetime.now().strftime('%Y.%m.%d')}",
            "generated_at": datetime.now().isoformat(),
            "total_tasks_analyzed": self.db.execute("SELECT COUNT(*) FROM task_records").fetchone()[0],
            "success_rate": self._overall_success_rate(),
            "proposals": proposals[:10],  # Top 10 improvements
        }
        
        # Save proposal
        for p in proposals[:10]:
            self.db.execute(
                "INSERT INTO version_proposals (version, title, description, priority) VALUES (?,?,?,?)",
                (version["version"], p["problem"][:100], p["fix"], p["priority"]),
            )
        self.db.commit()
        
        return version
    
    def _overall_success_rate(self) -> str:
        row = self.db.execute("SELECT COUNT(*), SUM(success) FROM task_records").fetchone()
        if row[0]:
            return f"{row[1]}/{row[0]} ({row[1]/row[0]*100:.0f}%)"
        return "N/A"
    
    def report(self) -> str:
        """Human-readable evolution report."""
        stats = self.db.execute("""
            SELECT COUNT(*), SUM(success), AVG(total_time), SUM(write_calls),
                   SUM(wasted_iterations), SUM(had_junk_files), SUM(needed_manual_fix)
            FROM task_records
        """).fetchone()
        
        if not stats[0]:
            return "No task data yet. Run some tasks first."
        
        lines = [
            "## ForgeFleet Evolution Report\n",
            f"Tasks: {stats[0]} total, {stats[1]} successful ({stats[1]/stats[0]*100:.0f}%)",
            f"Avg time: {stats[2]:.0f}s per task",
            f"Tool usage: {stats[3]} write_file calls",
            f"Wasted iterations: {stats[4]}",
            f"Junk files: {stats[5]} tasks",
            f"Manual fixes needed: {stats[6]} tasks",
            "",
        ]
        
        # Recent insights
        insights = self.db.execute(
            "SELECT category, finding, recommendation FROM insights ORDER BY created_at DESC LIMIT 5"
        ).fetchall()
        
        if insights:
            lines.append("### Recent Insights:")
            for i in insights:
                lines.append(f"  [{i[0]}] {i[1]}")
                lines.append(f"    → {i[2]}")
        
        # Pending proposals
        proposals = self.db.execute(
            "SELECT title, description, priority FROM version_proposals WHERE status='proposed' ORDER BY created_at DESC LIMIT 5"
        ).fetchall()
        
        if proposals:
            lines.append("\n### Improvement Proposals:")
            for p in proposals:
                icon = "🔴" if p[2] == "high" else "🟡"
                lines.append(f"  {icon} {p[0]}")
                lines.append(f"    Fix: {p[1]}")
        
        return "\n".join(lines)
    
    def close(self):
        self.db.close()
