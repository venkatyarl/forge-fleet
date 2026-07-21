"""Unit tests for the Lane-1.5 480B escalation dispatch flow.

The real routing/concurrency/fallback contract is implemented in Rust:
  - crates/ff-agent/src/work_item_dispatch.rs (should_attempt_lane15, task_prefers_cloud_lane)
  - crates/ff-agent/src/dispatch_concurrency.rs (MAX_480B_CONCURRENCY = 2)
  - crates/ff-control/src/llm_480b_wrapper.rs (Llm480bHttpWrapper.generate)

`Lane15Dispatcher` below is a test double that mirrors that contract in
Python: escalate to the 480B ring on a Lane-1 failure or moderate+
complexity, bounded by a process-wide semaphore capped at 2 concurrent
calls, falling back to a cloud CLI whenever the 480B lane is skipped,
saturated, or errors. The 480B endpoint and cloud CLI are injected
dependencies so tests can mock both without any network/process access.

Run with: pytest tests/test_lane15_escalation.py -v
"""

from __future__ import annotations

import threading
import time
from unittest.mock import MagicMock

MAX_480B_CONCURRENCY = 2
MODERATE_PLUS_COMPLEXITY = ("moderate", "complex")


def should_escalate_to_480b(lane1_failed: bool, complexity: str) -> bool:
    """Route to the 480B lane on a Lane-1 failure or moderate+ complexity."""
    return lane1_failed or complexity in MODERATE_PLUS_COMPLEXITY


class Lane15Dispatcher:
    """Test double for the Lane-1.5 escalation dispatcher.

    Calls the injected 480B endpoint when escalation applies, bounded by
    a semaphore capped at ``max_concurrency``. Falls back to the injected
    cloud CLI when the task doesn't escalate, the 480B lane is saturated,
    or the 480B call raises.
    """

    def __init__(self, llm_480b_endpoint, cloud_cli, max_concurrency: int = MAX_480B_CONCURRENCY):
        self._endpoint = llm_480b_endpoint
        self._cloud_cli = cloud_cli
        self._semaphore = threading.Semaphore(max_concurrency)

    def dispatch(self, prompt: str, lane1_failed: bool, complexity: str) -> dict:
        if not should_escalate_to_480b(lane1_failed, complexity):
            return {"provider": "cloud", "result": self._cloud_cli(prompt), "reason": "skipped_480b"}

        if not self._semaphore.acquire(blocking=False):
            return {"provider": "cloud", "result": self._cloud_cli(prompt), "reason": "480b_saturated"}

        try:
            result = self._endpoint(prompt)
        except Exception:
            return {"provider": "cloud", "result": self._cloud_cli(prompt), "reason": "480b_failed"}
        finally:
            self._semaphore.release()

        return {"provider": "480b", "result": result, "reason": None}


def make_dispatcher(endpoint=None, cloud_cli=None, max_concurrency: int = MAX_480B_CONCURRENCY):
    endpoint = endpoint or MagicMock(return_value="480b-result")
    cloud_cli = cloud_cli or MagicMock(return_value="cloud-result")
    return Lane15Dispatcher(endpoint, cloud_cli, max_concurrency), endpoint, cloud_cli


def test_lane1_failure_routes_to_480b():
    dispatcher, endpoint, cloud_cli = make_dispatcher()

    outcome = dispatcher.dispatch("fix the bug", lane1_failed=True, complexity="mechanical")

    assert outcome["provider"] == "480b"
    endpoint.assert_called_once_with("fix the bug")
    cloud_cli.assert_not_called()


def test_moderate_complexity_routes_to_480b_even_without_lane1_failure():
    dispatcher, endpoint, cloud_cli = make_dispatcher()

    outcome = dispatcher.dispatch("refactor module", lane1_failed=False, complexity="moderate")

    assert outcome["provider"] == "480b"
    endpoint.assert_called_once_with("refactor module")
    cloud_cli.assert_not_called()


def test_complex_complexity_routes_to_480b_even_without_lane1_failure():
    dispatcher, endpoint, cloud_cli = make_dispatcher()

    outcome = dispatcher.dispatch("rewrite subsystem", lane1_failed=False, complexity="complex")

    assert outcome["provider"] == "480b"
    endpoint.assert_called_once_with("rewrite subsystem")
    cloud_cli.assert_not_called()


def test_task_passing_lane1_with_mechanical_complexity_skips_to_cloud():
    dispatcher, endpoint, cloud_cli = make_dispatcher()

    outcome = dispatcher.dispatch("tiny tweak", lane1_failed=False, complexity="mechanical")

    assert outcome["provider"] == "cloud"
    assert outcome["reason"] == "skipped_480b"
    cloud_cli.assert_called_once_with("tiny tweak")
    endpoint.assert_not_called()


def test_semaphore_caps_concurrent_480b_calls_at_two():
    lock = threading.Lock()
    concurrent = 0
    max_concurrent = 0
    release_events = [threading.Event() for _ in range(4)]

    def endpoint(prompt):
        nonlocal concurrent, max_concurrent
        with lock:
            concurrent += 1
            max_concurrent = max(max_concurrent, concurrent)
        # Hold the permit until the test releases this specific call, so all
        # four attempts overlap and the cap is actually exercised.
        idx = int(prompt.rsplit("-", 1)[1])
        try:
            release_events[idx].wait(timeout=5)
            return f"480b:{idx}"
        finally:
            with lock:
                concurrent -= 1

    cloud_calls = []
    cloud_lock = threading.Lock()

    def cloud_cli(prompt):
        with cloud_lock:
            cloud_calls.append(prompt)
        return f"cloud:{prompt}"

    dispatcher = Lane15Dispatcher(endpoint, cloud_cli, max_concurrency=MAX_480B_CONCURRENCY)

    results = [None] * 4

    def worker(idx):
        results[idx] = dispatcher.dispatch(f"task-{idx}", lane1_failed=True, complexity="mechanical")

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(4)]
    for t in threads:
        t.start()
    # Give the first two workers a chance to acquire the semaphore and block
    # inside the endpoint before releasing them, so a third/fourth caller sees
    # the lane saturated.
    time.sleep(0.2)
    assert max_concurrent <= MAX_480B_CONCURRENCY, (
        f"480B endpoint saw {max_concurrent} concurrent calls, expected at most {MAX_480B_CONCURRENCY}"
    )
    for event in release_events:
        event.set()
    for t in threads:
        t.join(timeout=5)

    providers = sorted(r["provider"] for r in results)
    assert providers.count("480b") == MAX_480B_CONCURRENCY
    assert providers.count("cloud") == len(results) - MAX_480B_CONCURRENCY
    assert len(cloud_calls) == len(results) - MAX_480B_CONCURRENCY


def test_480b_failure_falls_back_to_cloud_cli():
    def endpoint(prompt):
        raise RuntimeError("480b endpoint unavailable")

    dispatcher, _, cloud_cli = make_dispatcher(endpoint=endpoint)

    outcome = dispatcher.dispatch("escalated task", lane1_failed=True, complexity="mechanical")

    assert outcome["provider"] == "cloud"
    assert outcome["reason"] == "480b_failed"
    cloud_cli.assert_called_once_with("escalated task")


def test_480b_saturation_falls_back_to_cloud_cli():
    hold = threading.Event()

    def endpoint(prompt):
        hold.wait(timeout=5)
        return "held"

    dispatcher, _, cloud_cli = make_dispatcher(endpoint=endpoint)

    holder_threads = [
        threading.Thread(
            target=dispatcher.dispatch, args=(f"hold-{i}",), kwargs={"lane1_failed": True, "complexity": "mechanical"}
        )
        for i in range(MAX_480B_CONCURRENCY)
    ]
    for t in holder_threads:
        t.start()
    time.sleep(0.2)

    overflow = dispatcher.dispatch("overflow", lane1_failed=True, complexity="mechanical")

    assert overflow["provider"] == "cloud"
    assert overflow["reason"] == "480b_saturated"
    cloud_cli.assert_called_once_with("overflow")

    hold.set()
    for t in holder_threads:
        t.join(timeout=5)
