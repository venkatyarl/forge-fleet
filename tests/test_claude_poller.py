from decimal import Decimal

from src.usage_pollers.claude_poller import (
    ClaudeClientConfig,
    parse_claude_usage,
    poll_claude_usage,
)


class Cursor:
    def __init__(self):
        self.call = None

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return None

    def execute(self, sql, params):
        self.call = (sql, params)


class Connection:
    def __init__(self):
        self.db_cursor = Cursor()
        self.committed = False

    def cursor(self):
        return self.db_cursor

    def commit(self):
        self.committed = True


def test_parse_oauth_usage_shape():
    usage = parse_claude_usage(
        {
            "seven_day": {"utilization": 66.4},
            "extra_usage": {"spent_today": "2.75"},
        }
    )
    assert usage.weekly_pct == 66
    assert usage.spent_today == Decimal("2.75")


def test_poll_updates_claude_bucket():
    connection = Connection()
    config = ClaudeClientConfig("secret", "https://usage.example")

    usage = poll_claude_usage(
        connection,
        config,
        lambda url, headers, timeout: {
            "weekly_pct": 42,
            "spent_today": 1.25,
        },
    )

    assert usage == parse_claude_usage({"weekly_pct": 42, "spent_today": 1.25})
    sql, params = connection.db_cursor.call
    assert "UPDATE cloud_budget_buckets" in sql
    assert "provider = 'claude'" in sql
    assert params == (42, Decimal("1.25"))
    assert connection.committed
