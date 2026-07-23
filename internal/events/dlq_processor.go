package internal/events

import (
	"errors"
	"fmt"
	"internal/ff_agent/nats_jetstream"
	"internal/ff_agent/nats_jetstream/ff_tasks_dlq_total"
	"sync"
)

// DlqProcessor manages dead-letter routing and metrics for task insertions
type DlqProcessor struct {
	// Subscription to ff.tasks.dlq for FF_TASKS
	sub *nats_jetstream.Subscription

	// Mutex for concurrent access
	mux sync.Mutex

	// Task identifiers to track status
	tasks map[string]*nats_jetstream.TaskInfo

	// Metric counter for DLQ
	ddlqCount int
}

func NewDlqProcessor() *DlqProcessor {
	return &DlqProcessor{
		sub:    nats_jetstream.Subscribe(ff_tasks_dlq_total.Subject),
		tasks:  make(map[string]*nats_jetstream.TaskInfo),
		ddlqCount: 0,
	}
}

func (p *DlqProcessor) Start() {
	go p.processMessages()
}

func (p *DlqProcessor) processMessages() {
	for {
		select {
		case msg := <-p.sub.GetMsg():
			if msg == nil {
				continue
			}

			// Extract task identifier
			taskId := msg.Header()["task_id"].(string)
			if taskId == "" {
				continue
			}

			// Update task status to DLQ_EXHAUSTED
			taskInfo, ok := p.tasks[taskId]
			if ok {
				taskInfo.Status = nats_jetstream.DLQ_EXHAUSTED
				p.mux.Lock()
				defer p.mux.Unlock()
				// Emit metric
				ff_tasks_dlq_total.Emit(taskInfo.Status)
			}

			// Store task info
			p.tasks[taskId] = &nats_jetstream.TaskInfo{
				TaskId: taskId,
				// Assume task_outbox status is updated via the observer
			}
		case <-p.sub.Closed():
			return
		}
	}
}

// TestDLQProcessor runs the test suite
func TestDLQProcessor() {
	// Implementation omitted for brevity
}

// DlqProcessor implements the necessary logic for the dead-letter processing
