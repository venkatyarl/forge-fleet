"""Poll Codex cloud usage and maintain its cloud budget bucket."""

from __future__ import annotations

import json
import math
import os
import subprocess
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any, Callable, Mapping, Optional


@dataclass(frozen=True)
class CodexUsage:
    weekly_pct: float
    spent_today: float
    source: str


def _number(value: Any) -> Optional[float]:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) else None


def parse_codex_usage(payload: Mapping[str, Any]) -> Optional[CodexUsage]:
    """Extract direct cost data from either a flat or ``usage`` response."""
    usage = payload.get("usage", payload)
    if not isinstance(usage, Mapping):
        return None

    weekly_pct = _number(
        usage.get("weekly_pct", usage.get("weekly_percent", usage.get("week_percent")))
    )
    spent_today = _number(
        usage.get("spent_today", usage.get("today_cost_usd", usage.get("daily_cost_usd")))
    )
    if weekly_pct is None or spent_today is None:
        return None
    return CodexUsage(
        weekly_pct=max(0.0, min(100.0, weekly_pct)),
        spent_today=max(0.0, spent_today),
        source="codex usage poller",
    )


def derive_fallback_usage(
    month_spend_usd: float, today_spend_usd: float, monthly_limit_usd: float
) -> CodexUsage:
    """Derive budget consumption from ``ff_interactions`` cost data."""
    values = (month_spend_usd, today_spend_usd, monthly_limit_usd)
    if not all(math.isfinite(value) for value in values) or monthly_limit_usd <= 0:
        raise ValueError("monthly_limit_usd must be positive and costs must be finite")
    return CodexUsage(
        weekly_pct=max(0.0, min(100.0, month_spend_usd / monthly_limit_usd * 100.0)),
        spent_today=max(0.0, today_spend_usd),
        source="codex usage poller (ff_interactions estimate)",
    )


def fetch_codex_cli_usage() -> Mapping[str, Any]:
    """Fetch direct usage from the Codex CLI, raising when it is unavailable."""
    completed = subprocess.run(
        ["codex", "usage", "--json"],
        check=True,
        capture_output=True,
        text=True,
        timeout=15,
    )
    payload = json.loads(completed.stdout)
    if not isinstance(payload, Mapping):
        raise ValueError("Codex usage response must be a JSON object")
    return payload


class CodexUsagePoller:
    """Synchronous poller suitable for the daemon's Python poller runner."""

    def __init__(
        self,
        connection: Any,
        monthly_limit_usd: Optional[float] = None,
        usage_fetcher: Callable[[], Mapping[str, Any]] = fetch_codex_cli_usage,
        now: Callable[[], datetime] = lambda: datetime.now(timezone.utc),
    ) -> None:
        self.connection = connection
        configured_limit = (
            monthly_limit_usd
            if monthly_limit_usd is not None
            else os.getenv("CODEX_MONTHLY_LIMIT_USD")
        )
        self.monthly_limit_usd = _number(configured_limit)
        self.usage_fetcher = usage_fetcher
        self.now = now

    def _fallback(self, cursor: Any) -> CodexUsage:
        if self.monthly_limit_usd is None or self.monthly_limit_usd <= 0:
            raise ValueError("CODEX_MONTHLY_LIMIT_USD must be a positive number")
        cursor.execute(
            """
            SELECT
                COALESCE(SUM(cost_usd) FILTER (
                    WHERE ts >= date_trunc('month', NOW())
                ), 0)::double precision,
                COALESCE(SUM(cost_usd) FILTER (
                    WHERE ts >= date_trunc('day', NOW())
                ), 0)::double precision
            FROM ff_interactions
            WHERE lower(engine) = 'codex'
            """
        )
        month_spend, today_spend = cursor.fetchone()
        return derive_fallback_usage(
            float(month_spend), float(today_spend), self.monthly_limit_usd
        )

    def poll(self) -> CodexUsage:
        """Poll direct usage, falling back to interaction costs, then upsert."""
        usage: Optional[CodexUsage] = None
        try:
            usage = parse_codex_usage(self.usage_fetcher())
        except (OSError, subprocess.SubprocessError, ValueError, json.JSONDecodeError):
            pass

        cursor = self.connection.cursor()
        try:
            if usage is None:
                usage = self._fallback(cursor)
            cursor.execute(
                """
                INSERT INTO cloud_budget_buckets
                    (provider, weekly_pct, spent_today, source,
                     last_success_at, updated_at)
                VALUES ('codex', %s, %s, %s, %s, %s)
                ON CONFLICT (provider) DO UPDATE SET
                    weekly_pct = EXCLUDED.weekly_pct,
                    spent_today = EXCLUDED.spent_today,
                    source = EXCLUDED.source,
                    last_success_at = EXCLUDED.last_success_at,
                    updated_at = EXCLUDED.updated_at
                """,
                (
                    usage.weekly_pct,
                    usage.spent_today,
                    usage.source,
                    self.now(),
                    self.now(),
                ),
            )
            self.connection.commit()
            return usage
        except Exception:
            self.connection.rollback()
            raise
        finally:
            cursor.close()


# Short name retained for poller registries that derive the class name from the
# module name.
CodexPoller = CodexUsagePoller
