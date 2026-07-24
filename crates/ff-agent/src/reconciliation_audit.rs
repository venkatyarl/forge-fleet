//! Audit trail for Pillar-4 reconciliation events.
//!
//! Every reconciliation action taken by the merge drain — conflicts resolved,
//! duplicate/racing claims skipped, successful merges, out-of-band status
//! reconciliations — is recorded as a [`ReconciliationEvent`] carrying a
//! trace id, a UTC timestamp, and an operation summary.
//!
//! Delivery is a channel + dedicated writer task. [`ReconciliationAuditor::record`]
//! is synchronous: it emits a structured tracing line and enqueues the event on
//! an unbounded channel, so the serial merge drain is never blocked (and never
//! sheds an event) waiting on the database. The writer task drains the channel
//! and persists each event through an [`AuditStore`] with bounded retries; a
//! permanently unpersistable event is dumped as canonical JSON to the error log
//! so the log stream remains a complete fallback sink. Dropping the last
//! auditor handle closes the channel, and awaiting the writer's `JoinHandle`
//! flushes everything already recorded — the drain does exactly that on
//! shutdown.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

/// Per-attempt cap on one audit INSERT so a hung connection cannot wedge the
/// writer task behind a single event.
const PERSIST_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
/// Attempts per event before the event is surrendered to the log stream.
const PERSIST_ATTEMPTS: u32 = 3;
/// Linear backoff unit between persist attempts (250ms, 500ms).
const PERSIST_BACKOFF: Duration = Duration::from_millis(250);

/// The reconciliation actions the merge drain can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconciliationAction {
    /// A merge conflict (DIRTY vs main, semantic compile failure, or a
    /// late-detected async-mergeability race) was resolved by resetting the
    /// item for a rebuild against current main.
    ConflictResolved,
    /// A duplicate action was skipped because another drain (or an earlier
    /// pass) already claimed it — e.g. a lost CI-rerun claim, an
    /// already-consumed semantic-reset, or a work_item row a racing drain
    /// already flipped.
    DuplicateSkipped,
    /// A PR was squash-merged and its work_item marked merged.
    MergeCompleted,
    /// A work_item stranded in `in_review` was reconciled with the actual
    /// out-of-band state of its PR (hand-merged or closed).
    StatusReconciled,
}

impl ReconciliationAction {
    /// Namespaced `event_type` written to `audit_log`.
    pub fn event_type(self) -> &'static str {
        match self {
            ReconciliationAction::ConflictResolved => "reconciliation.conflict_resolved",
            ReconciliationAction::DuplicateSkipped => "reconciliation.duplicate_skipped",
            ReconciliationAction::MergeCompleted => "reconciliation.merge_completed",
            ReconciliationAction::StatusReconciled => "reconciliation.status_reconciled",
        }
    }
}

/// One reconciliation event, uniquely traceable and self-describing.
#[derive(Debug, Clone)]
pub struct ReconciliationEvent {
    /// Unique id correlating the tracing line, the `audit_log` row, and any
    /// fallback error dump for this event.
    pub trace_id: Uuid,
    /// UTC time the action was recorded.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub action: ReconciliationAction,
    /// What the action applied to (work_item id, PR url, …).
    pub target: String,
    /// Human-readable operation summary.
    pub summary: String,
    /// Structured context (pr_url, queue_id, work_item_id, resolution, …).
    pub details: serde_json::Value,
}

impl ReconciliationEvent {
    /// Canonical JSON form — the exact payload persisted to
    /// `audit_log.details_json` and dumped to the error log on permanent
    /// persist failure.
    pub fn canonical_json(&self) -> String {
        serde_json::json!({
            "trace_id": self.trace_id.to_string(),
            "timestamp": self.timestamp.to_rfc3339(),
            "event_type": self.action.event_type(),
            "target": self.target,
            "summary": self.summary,
            "details": self.details,
        })
        .to_string()
    }
}

/// Persistence backend for reconciliation events. A trait so the whole
/// channel/retry/flush pipeline is exercised in CI without a database.
#[async_trait]
pub trait AuditStore: Send + Sync {
    async fn persist(&self, event: &ReconciliationEvent) -> anyhow::Result<()>;
}

/// Production store: one row per event in the live `audit_log` table
/// (all-TEXT columns; `timestamp` is RFC 3339).
pub struct PgAuditStore {
    pg: PgPool,
    worker_name: String,
}

impl PgAuditStore {
    pub fn new(pg: PgPool, worker_name: String) -> Self {
        Self { pg, worker_name }
    }
}

#[async_trait]
impl AuditStore for PgAuditStore {
    async fn persist(&self, event: &ReconciliationEvent) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO audit_log \
                (timestamp, event_type, actor, target, details_json, worker_name) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(event.timestamp.to_rfc3339())
        .bind(event.action.event_type())
        .bind("merge_drain")
        .bind(&event.target)
        .bind(event.canonical_json())
        .bind(&self.worker_name)
        .execute(&self.pg)
        .await?;
        Ok(())
    }
}

/// Handle for recording reconciliation events. Cloneable; when the last clone
/// is dropped the channel closes and the writer task drains and exits.
#[derive(Clone)]
pub struct ReconciliationAuditor {
    tx: mpsc::UnboundedSender<ReconciliationEvent>,
}

impl ReconciliationAuditor {
    /// Start the dedicated writer task. Returns the recording handle and the
    /// writer's `JoinHandle`; awaiting the handle after dropping every auditor
    /// clone flushes all recorded events.
    pub fn start(
        store: Arc<dyn AuditStore>,
    ) -> (ReconciliationAuditor, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<ReconciliationEvent>();
        let writer = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                persist_with_retry(store.as_ref(), &event).await;
            }
        });
        (ReconciliationAuditor { tx }, writer)
    }

    /// Record one reconciliation event: emit a structured tracing line and
    /// enqueue for persistence. Synchronous and non-blocking by design — an
    /// audit write must never stall the serial merge drain. Returns the
    /// event's trace id.
    pub fn record(
        &self,
        action: ReconciliationAction,
        target: &str,
        summary: &str,
        details: serde_json::Value,
    ) -> Uuid {
        let event = ReconciliationEvent {
            trace_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            action,
            target: target.to_string(),
            summary: summary.to_string(),
            details,
        };
        info!(
            trace_id = %event.trace_id,
            event_type = %event.action.event_type(),
            target = %event.target,
            summary = %event.summary,
            "reconciliation_audit"
        );
        let trace_id = event.trace_id;
        if self.tx.send(event).is_err() {
            // Writer already gone (shutdown race) — the tracing line above is
            // the durable record.
            warn!(%trace_id, "reconciliation_audit: writer stopped — event kept in log stream only");
        }
        trace_id
    }
}

/// Persist one event: bounded attempts, per-attempt timeout, linear backoff.
/// Permanent failure surrenders the full canonical JSON to the error log so no
/// event is ever silently lost.
async fn persist_with_retry(store: &dyn AuditStore, event: &ReconciliationEvent) {
    for attempt in 1..=PERSIST_ATTEMPTS {
        match tokio::time::timeout(PERSIST_ATTEMPT_TIMEOUT, store.persist(event)).await {
            Ok(Ok(())) => return,
            Ok(Err(e)) => warn!(
                trace_id = %event.trace_id,
                attempt,
                error = %e,
                "reconciliation_audit: persist attempt failed"
            ),
            Err(_) => warn!(
                trace_id = %event.trace_id,
                attempt,
                "reconciliation_audit: persist attempt timed out"
            ),
        }
        if attempt < PERSIST_ATTEMPTS {
            tokio::time::sleep(PERSIST_BACKOFF * attempt).await;
        }
    }
    error!(
        trace_id = %event.trace_id,
        event_json = %event.canonical_json(),
        "reconciliation_audit: persist failed permanently — event preserved in log stream"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn sample_event(action: ReconciliationAction) -> ReconciliationEvent {
        ReconciliationEvent {
            trace_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            action,
            target: "wi-123".to_string(),
            summary: "sample".to_string(),
            details: serde_json::json!({"pr_url": "https://github.com/o/r/pull/1"}),
        }
    }

    // ── Format tests (DB-free) ──────────────────────────────────────────────

    #[test]
    fn event_types_are_namespaced_per_action() {
        assert_eq!(
            ReconciliationAction::ConflictResolved.event_type(),
            "reconciliation.conflict_resolved"
        );
        assert_eq!(
            ReconciliationAction::DuplicateSkipped.event_type(),
            "reconciliation.duplicate_skipped"
        );
        assert_eq!(
            ReconciliationAction::MergeCompleted.event_type(),
            "reconciliation.merge_completed"
        );
        assert_eq!(
            ReconciliationAction::StatusReconciled.event_type(),
            "reconciliation.status_reconciled"
        );
    }

    #[test]
    fn canonical_json_carries_trace_id_timestamp_and_summary() {
        let event = sample_event(ReconciliationAction::MergeCompleted);
        let v: serde_json::Value = serde_json::from_str(&event.canonical_json()).unwrap();
        assert_eq!(v["trace_id"], event.trace_id.to_string());
        assert_eq!(v["event_type"], "reconciliation.merge_completed");
        assert_eq!(v["target"], "wi-123");
        assert_eq!(v["summary"], "sample");
        assert_eq!(v["details"]["pr_url"], "https://github.com/o/r/pull/1");
        // Timestamp round-trips as RFC 3339.
        let ts = v["timestamp"].as_str().unwrap();
        let parsed = chrono::DateTime::parse_from_rfc3339(ts).unwrap();
        assert_eq!(parsed.with_timezone(&chrono::Utc), event.timestamp);
    }

    #[tokio::test]
    async fn record_assigns_a_unique_trace_id_per_event() {
        let store = Arc::new(RecordingStore::default());
        let (auditor, writer) = ReconciliationAuditor::start(store);
        let a = auditor.record(
            ReconciliationAction::MergeCompleted,
            "wi-1",
            "s",
            serde_json::json!({}),
        );
        let b = auditor.record(
            ReconciliationAction::MergeCompleted,
            "wi-1",
            "s",
            serde_json::json!({}),
        );
        assert_ne!(a, b);
        drop(auditor);
        writer.await.unwrap();
    }

    // ── Structured tracing emission ─────────────────────────────────────────

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> SharedBuf {
            self.clone()
        }
    }

    #[tokio::test]
    async fn record_emits_a_structured_tracing_line() {
        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(buf.clone())
            .finish();

        let store = Arc::new(RecordingStore::default());
        let (auditor, writer) = ReconciliationAuditor::start(store);
        let dispatch = tracing::Dispatch::new(subscriber);
        // Sibling tests call `record` concurrently with no subscriber installed;
        // if one of them races the first-ever registration of `record`'s info!
        // callsite, tracing-core can cache the callsite as never-enabled even
        // though this thread's subscriber is active, and the event is skipped.
        // Rebuilding the interest cache under our dispatcher (and retrying while
        // a racing registration is still in flight) makes the capture reliable.
        let mut trace_id = Uuid::nil();
        for _ in 0..10 {
            trace_id = tracing::dispatcher::with_default(&dispatch, || {
                tracing::callsite::rebuild_interest_cache();
                auditor.record(
                    ReconciliationAction::ConflictResolved,
                    "wi-42",
                    "PR conflicted with advanced main — item reset for rebuild",
                    serde_json::json!({"queue_id": "q-1"}),
                )
            });
            if !buf.0.lock().unwrap().is_empty() {
                break;
            }
        }
        drop(auditor);
        writer.await.unwrap();

        let logged = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = logged
            .lines()
            .find(|l| l.contains(&trace_id.to_string()))
            .expect("record must emit a reconciliation_audit tracing line");
        assert!(line.contains("reconciliation_audit"));
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["fields"]["trace_id"], trace_id.to_string());
        assert_eq!(
            v["fields"]["event_type"],
            "reconciliation.conflict_resolved"
        );
        assert_eq!(v["fields"]["target"], "wi-42");
        assert_eq!(
            v["fields"]["summary"],
            "PR conflicted with advanced main — item reset for rebuild"
        );
    }

    // ── Persistence pipeline (DB-free via AuditStore) ───────────────────────

    #[derive(Default)]
    struct RecordingStore {
        persisted: Mutex<Vec<ReconciliationEvent>>,
    }

    #[async_trait]
    impl AuditStore for RecordingStore {
        async fn persist(&self, event: &ReconciliationEvent) -> anyhow::Result<()> {
            self.persisted.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn writer_persists_all_events_in_order_and_flushes_on_drop() {
        let store = Arc::new(RecordingStore::default());
        let (auditor, writer) = ReconciliationAuditor::start(store.clone());
        let mut trace_ids = Vec::new();
        for i in 0..5 {
            trace_ids.push(auditor.record(
                ReconciliationAction::DuplicateSkipped,
                &format!("wi-{i}"),
                "duplicate skipped",
                serde_json::json!({"i": i}),
            ));
        }
        // Dropping the last handle closes the channel; awaiting the writer
        // guarantees every already-recorded event was persisted (no loss).
        drop(auditor);
        writer.await.unwrap();

        let persisted = store.persisted.lock().unwrap();
        assert_eq!(
            persisted.iter().map(|e| e.trace_id).collect::<Vec<_>>(),
            trace_ids
        );
    }

    /// Fails the first `fail_first` persist calls, then succeeds.
    struct FlakyStore {
        calls: AtomicU32,
        fail_first: u32,
        persisted: Mutex<Vec<Uuid>>,
    }

    #[async_trait]
    impl AuditStore for FlakyStore {
        async fn persist(&self, event: &ReconciliationEvent) -> anyhow::Result<()> {
            if self.calls.fetch_add(1, Ordering::SeqCst) < self.fail_first {
                anyhow::bail!("transient db error");
            }
            self.persisted.lock().unwrap().push(event.trace_id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn writer_retries_transient_persist_failures() {
        let store = Arc::new(FlakyStore {
            calls: AtomicU32::new(0),
            fail_first: 2,
            persisted: Mutex::new(Vec::new()),
        });
        let (auditor, writer) = ReconciliationAuditor::start(store.clone());
        let trace_id = auditor.record(
            ReconciliationAction::MergeCompleted,
            "wi-9",
            "merged",
            serde_json::json!({}),
        );
        drop(auditor);
        writer.await.unwrap();

        assert_eq!(store.calls.load(Ordering::SeqCst), 3);
        assert_eq!(*store.persisted.lock().unwrap(), vec![trace_id]);
    }

    #[tokio::test]
    async fn poison_event_does_not_wedge_later_events() {
        // First event always fails (exhausts retries → error-log dump); the
        // second must still persist.
        let store = Arc::new(FlakyStore {
            calls: AtomicU32::new(0),
            fail_first: PERSIST_ATTEMPTS,
            persisted: Mutex::new(Vec::new()),
        });
        let (auditor, writer) = ReconciliationAuditor::start(store.clone());
        auditor.record(
            ReconciliationAction::ConflictResolved,
            "wi-poison",
            "poison",
            serde_json::json!({}),
        );
        let ok = auditor.record(
            ReconciliationAction::StatusReconciled,
            "wi-ok",
            "reconciled",
            serde_json::json!({}),
        );
        drop(auditor);
        writer.await.unwrap();

        assert_eq!(*store.persisted.lock().unwrap(), vec![ok]);
    }

    // ── Env-gated live-shape DB test ────────────────────────────────────────

    fn temp_db_urls() -> (String, String, String) {
        let base_url = std::env::var("FORGEFLEET_POSTGRES_URL")
            .or_else(|_| std::env::var("FORGEFLEET_DATABASE_URL"))
            .expect("FORGEFLEET_POSTGRES_URL or FORGEFLEET_DATABASE_URL must be set for DB tests");
        let (prefix, _) = base_url
            .rsplit_once('/')
            .expect("database URL must end with /<db>");
        let db_name = format!("ff_recon_audit_{}", Uuid::new_v4().simple());
        (
            format!("{prefix}/postgres"),
            format!("{prefix}/{db_name}"),
            db_name,
        )
    }

    #[tokio::test]
    async fn pg_store_writes_audit_log_rows() {
        if std::env::var("FORGEFLEET_POSTGRES_URL").is_err()
            && std::env::var("FORGEFLEET_DATABASE_URL").is_err()
        {
            eprintln!(
                "skipping pg_store_writes_audit_log_rows DB test: no FORGEFLEET_POSTGRES_URL/DATABASE_URL"
            );
            return;
        }
        let (admin_url, db_url, db_name) = temp_db_urls();
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect admin db");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create temp db");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .expect("connect temp db");
        // Mirror the LIVE audit_log shape (confirmed via ff db query):
        // id bigint identity; every other column TEXT.
        sqlx::raw_sql(
            "CREATE TABLE audit_log (
                 id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                 timestamp TEXT,
                 event_type TEXT,
                 actor TEXT,
                 target TEXT,
                 details_json TEXT,
                 worker_name TEXT
             )",
        )
        .execute(&pool)
        .await
        .expect("create audit_log mirror");

        let store = Arc::new(PgAuditStore::new(pool.clone(), "test-worker".to_string()));
        let (auditor, writer) = ReconciliationAuditor::start(store);
        let trace_id = auditor.record(
            ReconciliationAction::DuplicateSkipped,
            "wi-db",
            "CI rerun already claimed — duplicate rerun skipped",
            serde_json::json!({"queue_id": "q-7"}),
        );
        drop(auditor);
        writer.await.expect("writer flush");

        use sqlx::Row;
        let row = sqlx::query(
            "SELECT timestamp, event_type, actor, target, details_json, worker_name \
               FROM audit_log",
        )
        .fetch_one(&pool)
        .await
        .expect("fetch audit row");
        assert_eq!(
            row.get::<String, _>("event_type"),
            "reconciliation.duplicate_skipped"
        );
        assert_eq!(row.get::<String, _>("actor"), "merge_drain");
        assert_eq!(row.get::<String, _>("target"), "wi-db");
        assert_eq!(row.get::<String, _>("worker_name"), "test-worker");
        chrono::DateTime::parse_from_rfc3339(&row.get::<String, _>("timestamp"))
            .expect("timestamp column is RFC 3339");
        let details: serde_json::Value =
            serde_json::from_str(&row.get::<String, _>("details_json")).unwrap();
        assert_eq!(details["trace_id"], trace_id.to_string());
        assert_eq!(
            details["summary"],
            "CI rerun already claimed — duplicate rerun skipped"
        );
        assert_eq!(details["details"]["queue_id"], "q-7");

        pool.close().await;
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("drop temp db");
        admin.close().await;
    }
}
