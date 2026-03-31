"""Persistent execution tracking for ownership, collaboration, escalation, and model usage."""
from __future__ import annotations

import json
import time
from dataclasses import dataclass

from .db import connect


@dataclass
class ExecutionTracker:
    """Persist execution, event, and model-usage records to Postgres."""

    def __post_init__(self):
        self._init_db()

    def _init_db(self):
        with connect() as conn, conn.cursor() as cur:
            cur.execute(
                """
                CREATE TABLE IF NOT EXISTS task_execution (
                    ticket_id TEXT PRIMARY KEY,
                    current_owner TEXT,
                    owner_level TEXT,
                    state TEXT,
                    status_reason TEXT,
                    handoff_count INTEGER DEFAULT 0,
                    escalation_count INTEGER DEFAULT 0,
                    contributors_json TEXT DEFAULT '[]',
                    reviewers_json TEXT DEFAULT '[]',
                    escalation_path_json TEXT DEFAULT '[]',
                    last_model_json TEXT DEFAULT '{}',
                    updated_at DOUBLE PRECISION
                )
                """
            )
            cur.execute(
                """
                CREATE TABLE IF NOT EXISTS execution_events (
                    id BIGSERIAL PRIMARY KEY,
                    ticket_id TEXT,
                    event_type TEXT,
                    actor TEXT,
                    details_json TEXT DEFAULT '{}',
                    created_at DOUBLE PRECISION
                )
                """
            )
            cur.execute(
                """
                CREATE TABLE IF NOT EXISTS model_usage_events (
                    id BIGSERIAL PRIMARY KEY,
                    ticket_id TEXT,
                    stage TEXT,
                    model_name TEXT,
                    node_name TEXT,
                    role TEXT,
                    details_json TEXT DEFAULT '{}',
                    created_at DOUBLE PRECISION
                )
                """
            )

    def upsert_execution(self, ticket_id: str, current_owner: str, owner_level: str,
                         state: str, status_reason: str = "", handoff_count: int = 0,
                         escalation_count: int = 0, contributors: list | None = None,
                         reviewers: list | None = None, escalation_path: list | None = None,
                         last_model: dict | None = None):
        now = time.time()
        with connect() as conn, conn.cursor() as cur:
            cur.execute(
                """
                INSERT INTO task_execution (
                    ticket_id, current_owner, owner_level, state, status_reason,
                    handoff_count, escalation_count, contributors_json,
                    reviewers_json, escalation_path_json, last_model_json, updated_at
                ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON CONFLICT (ticket_id) DO UPDATE SET
                    current_owner=EXCLUDED.current_owner,
                    owner_level=EXCLUDED.owner_level,
                    state=EXCLUDED.state,
                    status_reason=EXCLUDED.status_reason,
                    handoff_count=EXCLUDED.handoff_count,
                    escalation_count=EXCLUDED.escalation_count,
                    contributors_json=EXCLUDED.contributors_json,
                    reviewers_json=EXCLUDED.reviewers_json,
                    escalation_path_json=EXCLUDED.escalation_path_json,
                    last_model_json=EXCLUDED.last_model_json,
                    updated_at=EXCLUDED.updated_at
                """,
                (
                    ticket_id, current_owner, owner_level, state, status_reason,
                    handoff_count, escalation_count,
                    json.dumps(contributors or []),
                    json.dumps(reviewers or []),
                    json.dumps(escalation_path or []),
                    json.dumps(last_model or {}),
                    now,
                ),
            )

    def log_event(self, ticket_id: str, event_type: str, actor: str, details: dict | None = None):
        with connect() as conn, conn.cursor() as cur:
            cur.execute(
                "INSERT INTO execution_events (ticket_id, event_type, actor, details_json, created_at) VALUES (%s, %s, %s, %s, %s)",
                (ticket_id, event_type, actor, json.dumps(details or {}), time.time()),
            )

    def log_model_usage(self, ticket_id: str, stage: str, model_name: str,
                        node_name: str, role: str, details: dict | None = None):
        with connect() as conn, conn.cursor() as cur:
            cur.execute(
                "INSERT INTO model_usage_events (ticket_id, stage, model_name, node_name, role, details_json, created_at) VALUES (%s, %s, %s, %s, %s, %s, %s)",
                (ticket_id, stage, model_name, node_name, role, json.dumps(details or {}), time.time()),
            )
