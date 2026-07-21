// Package db defines persistence abstractions used by ForgeFleet.
package db

import (
	"context"
	"time"
)

// OutboxEvent is an event waiting to be delivered by the offline sync workflow.
type OutboxEvent struct {
	EventID   string
	EventType string
	Payload   []byte
	Status    string
	CreatedAt time.Time
}

// OutboxRepository persists and updates events independently of the underlying
// ORM or SQL driver.
type OutboxRepository interface {
	Create(ctx context.Context, event *OutboxEvent) error
	ListPending(ctx context.Context) ([]OutboxEvent, error)
	UpdateStatus(ctx context.Context, eventID, status string) error
}
