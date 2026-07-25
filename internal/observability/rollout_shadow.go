package observability

import (
	"bytes"
	"encoding/json"
	"expvar"
	"log/slog"
)

var rolloutShadowMismatchTotal = expvar.NewInt("ff_rollout_shadow_mismatch_total")

// LogShadowParity compares a NATS outbox payload with the authoritative
// Postgres state. It is observational only and does not affect message
// processing or the native LISTEN/NOTIFY path.
func LogShadowParity(outboxPayload, pgState any) {
	outboxJSON, outboxErr := canonicalJSON(outboxPayload)
	pgJSON, pgErr := canonicalJSON(pgState)
	if outboxErr == nil && pgErr == nil && bytes.Equal(outboxJSON, pgJSON) {
		return
	}

	rolloutShadowMismatchTotal.Add(1)
	slog.Warn(
		"rollout shadow parity mismatch",
		"metric", "ff_rollout_shadow_mismatch_total",
		"outbox_payload", outboxPayload,
		"postgres_state", pgState,
		"outbox_error", outboxErr,
		"postgres_error", pgErr,
	)
}

func canonicalJSON(value any) ([]byte, error) {
	switch value := value.(type) {
	case []byte:
		var decoded any
		if err := json.Unmarshal(value, &decoded); err != nil {
			return nil, err
		}
		return json.Marshal(decoded)
	case string:
		var decoded any
		if err := json.Unmarshal([]byte(value), &decoded); err != nil {
			return nil, err
		}
		return json.Marshal(decoded)
	default:
		return json.Marshal(value)
	}
}
