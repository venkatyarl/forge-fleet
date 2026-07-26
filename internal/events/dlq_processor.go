package events

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/prometheus/client_golang/prometheus"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/metric"

	"github.com/venkatyarl/forge-fleet/internal/database"
)

type DLQProcessor struct {
	nc         *nats.Conn
	database   *database.Database
	meter     metric.Meter
	dlqTotal  metric.Int64Counter
}

func NewDLQProcessor(nc *nats.Conn, db *database.Database, meter metric.Meter) *DLQProcessor {
	dlqTotal, err := meter.Int64Counter(
		"ff_tasks_dlq_total",
		metric.WithDescription("Total number of tasks sent to dead-letter queue"),
	)
	if err != nil {
		log.Printf("failed to create dlq_total metric: %v", err)
	}

	return &DLQProcessor{
		nc:        nc,
		database:  db,
		meter:     meter,
		dlqTotal:  dlqTotal,
	}
}

func (p *DLQProcessor) Start(ctx context.Context) error {
	_, err := p.nc.Subscribe("ff.tasks.dlq", p.handleDLQMessage)
	if err != nil {
		return fmt.Errorf("failed to subscribe to ff.tasks.dlq: %w", err)
	}

	log.Println("DLQ processor started and subscribed to ff.tasks.dlq")
	return nil
}

func (p *DLQProcessor) handleDLQMessage(msg *nats.Msg) {
	var payload struct {
		TaskID string `json:"task_id"`
	}

	if err := json.Unmarshal(msg.Data, &payload); err != nil {
		log.Printf("failed to unmarshal DLQ message: %v", err)
		return
	}

	// Update task outbox status to DLQ_EXHAUSTED
	if err := p.database.UpdateTaskOutboxStatus(context.Background(), payload.TaskID, database.TaskOutboxStatusDLQExhausted); err != nil {
		log.Printf("failed to update task outbox status for task %s: %v", payload.TaskID, err)
		return
	}

	// Emit metric
	if p.dlqTotal != nil {
		p.dlqTotal.Add(context.Background(), 1,
			metric.WithAttributes(attribute.String("task_id", payload.TaskID)),
		)
	}

	log.Printf("Task %s moved to DLQ", payload.TaskID)
}

// RegisterDLQConsumer registers a JetStream consumer that pushes messages to ff.tasks.dlq
// when MaxDeliver is reached. This should be called when setting up the FF_TASKS stream.
func RegisterDLQConsumer(js nats.JetStreamContext, streamName string, consumerName string) error {
	_, err := js.AddConsumer(streamName, &nats.ConsumerConfig{
		Durable:   consumerName,
		DeliverPolicy: nats.DeliverPolicyAll,
		FilterStream: true,
		FilterSubject: "ff.tasks.>",
		DeliverSubject: "ff.tasks.dlq",
		MaxDeliver: 3, // This should match your retry policy
		AckPolicy:   nats.AckExplicit,
	})
	if err != nil {
		return fmt.Errorf("failed to add DLQ consumer: %w", err)
	}

	log.Printf("DLQ consumer %s registered for stream %s", consumerName, streamName)
	return nil
}
