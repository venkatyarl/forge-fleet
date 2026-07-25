"""Poll Claude usage and refresh its cloud budget bucket.

Claude Code exposes subscription usage as percentages (for example,
``seven_day.utilization``).  The poller also accepts a direct ``weekly_pct``
field so it can be used with an operator-provided proxy for Anthropic's usage
API.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from decimal import ROUND_HALF_UP, Decimal
from typing import Any, Callable, Mapping
from urllib.request import Request, urlopen

DEFAULT_USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
DEFAULT_TIMEOUT_SECONDS = 15.0


@dataclass(frozen=True)
class ClaudeUsage:
    """Values written to ``cloud_budget_buckets``."""

    weekly_pct: int
    spent_today: Decimal


@dataclass(frozen=True)
class ClaudeClientConfig:
    """Configuration for the Claude usage API client."""

    api_key: str
    usage_url: str = DEFAULT_USAGE_URL
    timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS

    @classmethod
    def from_env(cls) -> "ClaudeClientConfig":
        api_key = os.getenv("CLAUDE_API_KEY") or os.getenv("ANTHROPIC_API_KEY")
        if not api_key:
            raise ValueError("CLAUDE_API_KEY or ANTHROPIC_API_KEY is required")
        return cls(
            api_key=api_key,
            usage_url=os.getenv("CLAUDE_USAGE_API_URL", DEFAULT_USAGE_URL),
            timeout_seconds=float(
                os.getenv("CLAUDE_USAGE_TIMEOUT_SECONDS", DEFAULT_TIMEOUT_SECONDS)
            ),
        )


def _number(value: Any, field: str) -> Decimal:
    if isinstance(value, bool):
        raise ValueError(f"{field} must be numeric")
    try:
        result = Decimal(str(value))
    except Exception as exc:
        raise ValueError(f"{field} must be numeric") from exc
    if not result.is_finite():
        raise ValueError(f"{field} must be finite")
    return result


def parse_claude_usage(payload: Mapping[str, Any] | str | bytes) -> ClaudeUsage:
    """Convert either Claude's OAuth response or a normalized proxy response."""

    if isinstance(payload, (str, bytes)):
        payload = json.loads(payload)
    if not isinstance(payload, Mapping):
        raise ValueError("Claude usage response must be a JSON object")

    weekly_value = payload.get("weekly_pct")
    if weekly_value is None:
        weekly = payload.get("seven_day")
        if isinstance(weekly, Mapping):
            weekly_value = weekly.get("utilization")
    if weekly_value is None:
        raise ValueError("Claude usage response is missing weekly utilization")

    weekly_pct = int(
        _number(weekly_value, "weekly_pct").quantize(
            Decimal("1"), rounding=ROUND_HALF_UP
        )
    )
    weekly_pct = max(0, min(100, weekly_pct))

    spent_value = payload.get("spent_today")
    if spent_value is None:
        daily = payload.get("daily")
        if isinstance(daily, Mapping):
            spent_value = daily.get("spent") or daily.get("cost")
    if spent_value is None:
        extra = payload.get("extra_usage")
        if isinstance(extra, Mapping):
            spent_value = extra.get("spent_today")
            if spent_value is None:
                spent_value = extra.get("used_credits")
    spent_today = _number(spent_value or 0, "spent_today")
    return ClaudeUsage(weekly_pct=weekly_pct, spent_today=max(Decimal(0), spent_today))


def _default_http_get(url: str, headers: Mapping[str, str], timeout: float) -> Any:
    request = Request(url, headers=dict(headers), method="GET")
    with urlopen(request, timeout=timeout) as response:
        return response.read()


def fetch_claude_usage(
    config: ClaudeClientConfig,
    http_get: Callable[[str, Mapping[str, str], float], Any] = _default_http_get,
) -> ClaudeUsage:
    """Fetch and parse one Claude usage snapshot."""

    payload = http_get(
        config.usage_url,
        {
            "Authorization": f"Bearer {config.api_key}",
            "Accept": "application/json",
            "anthropic-beta": "oauth-2025-04-20",
            "User-Agent": "forgefleet-claude-usage-poller",
        },
        config.timeout_seconds,
    )
    # Test clients and lightweight API wrappers commonly return decoded JSON.
    if hasattr(payload, "json"):
        payload = payload.json()
    return parse_claude_usage(payload)


def update_cloud_budget_bucket(connection: Any, usage: ClaudeUsage) -> None:
    """Update the existing Claude bucket using a PEP-249 connection."""

    with connection.cursor() as cursor:
        cursor.execute(
            """
            UPDATE cloud_budget_buckets
               SET weekly_pct = %s,
                   spent_today = %s,
                   last_success_at = NOW(),
                   source = 'claude usage poller',
                   updated_at = NOW()
             WHERE provider = 'claude'
            """,
            (usage.weekly_pct, usage.spent_today),
        )
    connection.commit()


def poll_claude_usage(
    connection: Any,
    config: ClaudeClientConfig | None = None,
    http_get: Callable[[str, Mapping[str, str], float], Any] = _default_http_get,
) -> ClaudeUsage:
    """Fetch Claude metrics and persist them in ``cloud_budget_buckets``."""

    usage = fetch_claude_usage(config or ClaudeClientConfig.from_env(), http_get)
    update_cloud_budget_bucket(connection, usage)
    return usage


# A concise name for schedulers that import pollers uniformly.
poll = poll_claude_usage
