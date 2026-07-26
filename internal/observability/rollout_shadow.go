package observability

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"expvar"
	"io"
	"log/slog"
	"math/big"
	"reflect"
)

var rolloutShadowMismatchTotal = expvar.NewInt("ff_rollout_shadow_mismatch_total")

// LogShadowParity compares a NATS outbox payload with authoritative Postgres
// state. It is observational only: a mismatch cannot affect LISTEN/NOTIFY
// delivery, acknowledgement, or database state.
func LogShadowParity(outboxPayload, pgState json.RawMessage) bool {
	outbox, outboxErr := decodeShadowJSON(outboxPayload)
	postgres, postgresErr := decodeShadowJSON(pgState)

	reason := ""
	switch {
	case outboxErr != nil:
		reason = "invalid_outbox_json"
	case postgresErr != nil:
		reason = "invalid_pg_json"
	case !shadowValuesEqual(outbox, postgres):
		reason = "state_mismatch"
	default:
		return true
	}

	rolloutShadowMismatchTotal.Add(1)
	slog.Warn(
		"rollout shadow parity mismatch",
		"event", "rollout_shadow_mismatch",
		"reason", reason,
		"outbox_sha256", shadowPayloadHash(outboxPayload),
		"pg_sha256", shadowPayloadHash(pgState),
	)
	return false
}

func decodeShadowJSON(payload json.RawMessage) (any, error) {
	if len(bytes.TrimSpace(payload)) == 0 {
		return nil, nil
	}

	decoder := json.NewDecoder(bytes.NewReader(payload))
	decoder.UseNumber()

	var value any
	if err := decoder.Decode(&value); err != nil {
		return nil, err
	}

	var trailing any
	if err := decoder.Decode(&trailing); !errors.Is(err, io.EOF) {
		if err == nil {
			err = errors.New("multiple JSON values")
		}
		return nil, err
	}
	return value, nil
}

func shadowValuesEqual(left, right any) bool {
	leftNumber, leftIsNumber := left.(json.Number)
	rightNumber, rightIsNumber := right.(json.Number)
	if leftIsNumber || rightIsNumber {
		if !leftIsNumber || !rightIsNumber {
			return false
		}
		leftRat, leftOK := new(big.Rat).SetString(leftNumber.String())
		rightRat, rightOK := new(big.Rat).SetString(rightNumber.String())
		return leftOK && rightOK && leftRat.Cmp(rightRat) == 0
	}

	switch typedLeft := left.(type) {
	case []any:
		typedRight, ok := right.([]any)
		if !ok || len(typedLeft) != len(typedRight) {
			return false
		}
		for index := range typedLeft {
			if !shadowValuesEqual(typedLeft[index], typedRight[index]) {
				return false
			}
		}
		return true
	case map[string]any:
		typedRight, ok := right.(map[string]any)
		if !ok || len(typedLeft) != len(typedRight) {
			return false
		}
		for key, leftValue := range typedLeft {
			rightValue, exists := typedRight[key]
			if !exists || !shadowValuesEqual(leftValue, rightValue) {
				return false
			}
		}
		return true
	default:
		return reflect.DeepEqual(left, right)
	}
}

func shadowPayloadHash(payload json.RawMessage) string {
	sum := sha256.Sum256(payload)
	return hex.EncodeToString(sum[:6])
}
