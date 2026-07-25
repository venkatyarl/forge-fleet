package observability

import "testing"

func TestShadowParity(t *testing.T) {
	start := rolloutShadowMismatchTotal.Value()

	LogShadowParity(
		[]byte(`{"status":"running","task_id":"task-1"}`),
		map[string]any{"task_id": "task-1", "status": "running"},
	)
	if got := rolloutShadowMismatchTotal.Value(); got != start {
		t.Fatalf("matching states incremented mismatch counter: got %d, want %d", got, start)
	}

	LogShadowParity(
		[]byte(`{"status":"queued","task_id":"task-1"}`),
		map[string]any{"task_id": "task-1", "status": "running"},
	)
	if got := rolloutShadowMismatchTotal.Value(); got != start+1 {
		t.Fatalf("mismatched states did not increment counter: got %d, want %d", got, start+1)
	}
}
