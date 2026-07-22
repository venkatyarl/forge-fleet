package internal/events

import (
	"context"
	"fmt"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/nats-io/go-nats"
	"github.com/nats-io/go-nats/jetstream"
	"github.com/venkatyarl/forge-fleet/pkg/ffdb"
	"github.com/venkatyarl/forge-fleet/pkg/ffdb/ff_tasks"
)

// RunSchedulerConsumer attaches to JetStream stream FF_TASKS as durable consumer
// using pull mode. It consumes messages with explicit acks and claim, tracks
// attempt_metadata and enforces bounded redelivery limits.
func RunSchedulerConsumer(ctx context.Context, natsConn nats.Conn, dbPool *ffdb.DB) {
	// Create JetStream consumer
	consumer, err := jetstream.NewConsumer(natsConn)
	if err != nil {
		logger.Fatalf("failed to create JetStream consumer: %v", err)
	}
	defer consumer.Close()

	// Attach to durable stream FF_TASKS
	stream, err := consumer.Attach("FF_TASKS", jetstream.Durable("FF_TASKS"))
	if err != nil {
		logger.Fatalf("failed to attach to JetStream stream: %v", err)
	}
	defer stream.Close()

	// Set up event loop with bounded redelivery
	stream.SetRedeliveryLimit(2)
	stream.SetRedeliveryTimeout(10 * time.Second)

	// Initialize consumer state
	var state struct {
		Attempt int
		Claimed bool
	}
	var mu sync.Mutex
	var attemptMetadata struct {
		Attempt int
		Claimed bool
	}

	// Process messages with explicit acks and claim
	for {
		msg, err := stream.Pull(context.Background(), nil)
		if err != nil {
			logger.Warn("failed to pull message: %v", err)
			continue
		}

		// Handle message
		msgID := msg.ID
		msgBody := msg.Data

		// Check if message is already claimed
		mu.Lock()
		if attemptMetadata.Claimed {
			mu.Unlock()
			logger.Warn("message already claimed: %s", msgID)
			msg.Ack()
			continue
		}

		// Check if message is expired
		if msg.Acked() {
			logger.Warn("message expired: %s", msgID)
			msg.Ack()
			mu.Unlock()
			continue
		}

		// Attempt to claim the message
		mu.Lock()
		if attemptMetadata.Claimed {
			mu.Unlock()
			logger.Warn("message already claimed: %s", msgID)
			msg.Ack()
			continue
		}

		// Try to claim
		attemptMetadata.Attempt++
		attemptMetadata.Claimed = true
		mu.Unlock()

		// Process message
		logger.Trace("processing message: %s", msgID)
		// Replace with actual message processing logic
		// Example:
		// err := ff_tasks.ProcessTask(msgID, msgBody)
		// if err != nil {
		// 	// Handle error
		// }

		// Ack the message
		msg.Ack()

		// Update task status in database
		updateStmt := fmt.Sprintf("UPDATE task_outbox SET status = 'CONSUMED' WHERE id = %s", msgID)
		_, err = dbPool.ExecContext(context.Background(), updateStmt)
		if err != nil {
			logger.Warn("failed to update task status: %v", err)
		}

		// Check authoritative state
		authoritative, err := ffdb.GetAuthoritativeState(dbPool, msgID)
		if err != nil {
			logger.Warn("failed to get authoritative state: %v", err)
		} else {
			logger.Trace("authoritative state: %v", authoritative)
		}

		// End of message processing
	}

}
