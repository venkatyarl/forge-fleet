"""Model governance and SQLite-backed execution memory for ForgeFleet.

Phase 1:
- register models and nodes
- record task/model runs
- store simple recommendations by task type
- provide a canonical place for local-vs-paid model strategy data
"""
from __future__ import annotations

import json
import os
import sqlite3
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


@dataclass
class ModelRecord:
    model_id: str
    family: str = ""
    provider: str = ""
    tier: int = 0
    is_local: bool = True
    node: str = ""
    endpoint: str = ""
    status: str = "active"
    metadata: Optional[dict] = None


@dataclass
class TaskRunRecord:
    task_type: str
    mode: str
    model_id: str
    node: str = ""
    prompt_summary: str = ""
    success: bool = True
    latency_ms: int = 0
    input_tokens: int = 0
    output_tokens: int = 0
    cost_estimate: float = 0.0
    score: Optional[float] = None
    metadata: Optional[dict] = None
    timestamp: float = 0.0


class ModelGovernance:
    """Canonical SQLite-backed model governance store.

    This is intentionally simple for phase 1. It gives ForgeFleet one place
    to remember which models exist, how they performed, and what the current
    task-type recommendation should be.
    """

    def __init__(self, db_path: Optional[str] = None):
        self.db_path = db_path or str(Path.home() / '.forgefleet' / 'governance.db')
        os.makedirs(os.path.dirname(self.db_path), exist_ok=True)
        self._init_db()

    def _connect(self):
        return sqlite3.connect(self.db_path)

    def _init_db(self):
        with self._connect() as conn:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS models (
                    model_id TEXT PRIMARY KEY,
                    family TEXT,
                    provider TEXT,
                    tier INTEGER DEFAULT 0,
                    is_local INTEGER DEFAULT 1,
                    node TEXT,
                    endpoint TEXT,
                    status TEXT DEFAULT 'active',
                    metadata TEXT,
                    created_at REAL DEFAULT (strftime('%s','now'))
                )
            """)
            conn.execute("""
                CREATE TABLE IF NOT EXISTS task_runs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    task_type TEXT NOT NULL,
                    mode TEXT NOT NULL,
                    model_id TEXT NOT NULL,
                    node TEXT,
                    prompt_summary TEXT,
                    success INTEGER NOT NULL,
                    latency_ms INTEGER DEFAULT 0,
                    input_tokens INTEGER DEFAULT 0,
                    output_tokens INTEGER DEFAULT 0,
                    cost_estimate REAL DEFAULT 0,
                    score REAL,
                    metadata TEXT,
                    timestamp REAL NOT NULL
                )
            """)
            conn.execute("""
                CREATE TABLE IF NOT EXISTS task_recommendations (
                    task_type TEXT PRIMARY KEY,
                    recommended_model_id TEXT,
                    recommended_mode TEXT DEFAULT 'single',
                    confidence REAL DEFAULT 0,
                    rationale TEXT,
                    updated_at REAL NOT NULL
                )
            """)
            conn.execute("CREATE INDEX IF NOT EXISTS idx_task_runs_task_type ON task_runs(task_type)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_task_runs_model_id ON task_runs(model_id)")
            conn.execute("CREATE INDEX IF NOT EXISTS idx_task_runs_timestamp ON task_runs(timestamp)")

    def register_model(self, record: ModelRecord):
        with self._connect() as conn:
            conn.execute(
                """
                INSERT INTO models (model_id, family, provider, tier, is_local, node, endpoint, status, metadata)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(model_id) DO UPDATE SET
                    family=excluded.family,
                    provider=excluded.provider,
                    tier=excluded.tier,
                    is_local=excluded.is_local,
                    node=excluded.node,
                    endpoint=excluded.endpoint,
                    status=excluded.status,
                    metadata=excluded.metadata
                """,
                (
                    record.model_id,
                    record.family,
                    record.provider,
                    record.tier,
                    1 if record.is_local else 0,
                    record.node,
                    record.endpoint,
                    record.status,
                    json.dumps(record.metadata or {}),
                ),
            )

    def record_task_run(self, run: TaskRunRecord):
        ts = run.timestamp or time.time()
        with self._connect() as conn:
            conn.execute(
                """
                INSERT INTO task_runs (
                    task_type, mode, model_id, node, prompt_summary,
                    success, latency_ms, input_tokens, output_tokens,
                    cost_estimate, score, metadata, timestamp
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    run.task_type,
                    run.mode,
                    run.model_id,
                    run.node,
                    run.prompt_summary,
                    1 if run.success else 0,
                    run.latency_ms,
                    run.input_tokens,
                    run.output_tokens,
                    run.cost_estimate,
                    run.score,
                    json.dumps(run.metadata or {}),
                    ts,
                ),
            )

    def set_recommendation(self, task_type: str, model_id: str, mode: str = 'single',
                           confidence: float = 0.0, rationale: str = ''):
        with self._connect() as conn:
            conn.execute(
                """
                INSERT INTO task_recommendations (task_type, recommended_model_id, recommended_mode, confidence, rationale, updated_at)
                VALUES (?, ?, ?, ?, ?, ?)
                ON CONFLICT(task_type) DO UPDATE SET
                    recommended_model_id=excluded.recommended_model_id,
                    recommended_mode=excluded.recommended_mode,
                    confidence=excluded.confidence,
                    rationale=excluded.rationale,
                    updated_at=excluded.updated_at
                """,
                (task_type, model_id, mode, confidence, rationale, time.time()),
            )

    def get_recommendation(self, task_type: str) -> Optional[dict]:
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT task_type, recommended_model_id, recommended_mode, confidence, rationale, updated_at
                FROM task_recommendations WHERE task_type = ?
                """,
                (task_type,),
            ).fetchone()
            if not row:
                return None
            return {
                'task_type': row[0],
                'model_id': row[1],
                'mode': row[2],
                'confidence': row[3],
                'rationale': row[4],
                'updated_at': row[5],
            }

    def summarize_model_performance(self, task_type: str) -> list[dict]:
        with self._connect() as conn:
            rows = conn.execute(
                """
                SELECT model_id,
                       COUNT(*) as runs,
                       AVG(CASE WHEN success = 1 THEN 1.0 ELSE 0.0 END) as success_rate,
                       AVG(latency_ms) as avg_latency_ms,
                       AVG(COALESCE(score, 0)) as avg_score,
                       AVG(cost_estimate) as avg_cost
                FROM task_runs
                WHERE task_type = ?
                GROUP BY model_id
                ORDER BY avg_score DESC, success_rate DESC, avg_latency_ms ASC
                """,
                (task_type,),
            ).fetchall()
            return [
                {
                    'model_id': r[0],
                    'runs': r[1],
                    'success_rate': round(r[2] or 0, 3),
                    'avg_latency_ms': int(r[3] or 0),
                    'avg_score': round(r[4] or 0, 3),
                    'avg_cost': round(r[5] or 0, 4),
                }
                for r in rows
            ]

    def recommend_from_history(self, task_type: str) -> Optional[dict]:
        leaderboard = self.summarize_model_performance(task_type)
        if not leaderboard:
            return self.get_recommendation(task_type)
        best = leaderboard[0]
        return {
            'task_type': task_type,
            'model_id': best['model_id'],
            'mode': 'single',
            'confidence': min(0.95, 0.5 + (best['runs'] * 0.05)),
            'rationale': f"Best historical performer for {task_type}: success={best['success_rate']}, score={best['avg_score']}, latency_ms={best['avg_latency_ms']}",
            'history': leaderboard[:5],
        }
