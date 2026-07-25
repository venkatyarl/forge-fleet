from src.usage_pollers.codex_poller import (
    CodexUsagePoller,
    derive_fallback_usage,
    parse_codex_usage,
)


class Cursor:
    def __init__(self, row=(25.0, 3.5)):
        self.row = row
        self.calls = []

    def execute(self, sql, params=None):
        self.calls.append((sql, params))

    def fetchone(self):
        return self.row

    def close(self):
        pass


class Connection:
    def __init__(self):
        self.cursor_instance = Cursor()
        self.committed = False
        self.rolled_back = False

    def cursor(self):
        return self.cursor_instance

    def commit(self):
        self.committed = True

    def rollback(self):
        self.rolled_back = True


def test_direct_usage_is_parsed_and_clamped():
    usage = parse_codex_usage({"usage": {"weekly_pct": 110, "spent_today": 4.25}})
    assert usage is not None
    assert usage.weekly_pct == 100
    assert usage.spent_today == 4.25


def test_fallback_derives_percentage_from_monthly_limit():
    usage = derive_fallback_usage(25.0, 3.5, 100.0)
    assert usage.weekly_pct == 25
    assert usage.spent_today == 3.5


def test_poller_falls_back_to_ff_interactions_and_updates_bucket():
    connection = Connection()
    poller = CodexUsagePoller(
        connection,
        monthly_limit_usd=100,
        usage_fetcher=lambda: {},
    )

    usage = poller.poll()

    assert usage.weekly_pct == 25
    assert usage.spent_today == 3.5
    assert connection.committed
    assert "FROM ff_interactions" in connection.cursor_instance.calls[0][0]
    update_sql, params = connection.cursor_instance.calls[1]
    assert "cloud_budget_buckets" in update_sql
    assert params[:3] == (
        25,
        3.5,
        "codex usage poller (ff_interactions estimate)",
    )
