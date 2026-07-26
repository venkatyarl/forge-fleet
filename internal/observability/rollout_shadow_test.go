package observability

import (
	"encoding/json"
	"testing"
)

func TestShadowParity(t *testing.T) {
	tests := []struct {
		name    string
		outbox  json.RawMessage
		postgres json.RawMessage
		want    bool
	}{
		{name: "object order and whitespace", outbox: json.RawMessage(`{"id":1,"active":true}`), postgres: json.RawMessage(` { "active": true, "id": 1.0 } `), want: true},
		{name: "absent representations", outbox: nil, postgres: json.RawMessage(`null`), want: true},
		{name: "nested numeric precision", outbox: json.RawMessage(`{"value":9007199254740993}`), postgres: json.RawMessage(`{"value":9007199254740993.0}`), want: true},
		{name: "array order matters", outbox: json.RawMessage(`[1,2]`), postgres: json.RawMessage(`[2,1]`), want: false},
		{name: "missing differs from null", outbox: json.RawMessage(`{}`), postgres: json.RawMessage(`{"value":null}`), want: false},
		{name: "invalid outbox", outbox: json.RawMessage(`{`), postgres: json.RawMessage(`{}`), want: false},
		{name: "invalid postgres", outbox: json.RawMessage(`{}`), postgres: json.RawMessage(`{`), want: false},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := LogShadowParity(test.outbox, test.postgres); got != test.want {
				t.Fatalf("LogShadowParity() = %v, want %v", got, test.want)
			}
		})
	}
}

func TestShadowParityMismatchMetric(t *testing.T) {
	before := rolloutShadowMismatchTotal.Value()
	if LogShadowParity(json.RawMessage(`{"state":"old"}`), json.RawMessage(`{"state":"new"}`)) {
		t.Fatal("LogShadowParity() = true, want false")
	}
	if got := rolloutShadowMismatchTotal.Value() - before; got != 1 {
		t.Fatalf("ff_rollout_shadow_mismatch_total increment = %d, want 1", got)
	}
}
