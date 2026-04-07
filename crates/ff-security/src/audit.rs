use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEventInput {
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub sequence: u64,
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub metadata: serde_json::Value,
    pub prev_hash: Option<String>,
    pub hash: String,
}

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("hash-chain lock poisoned")]
    LockPoisoned,

    #[error("audit chain verification failed at sequence {sequence}")]
    ChainVerificationFailed { sequence: u64 },
}

pub type AuditResult<T> = std::result::Result<T, AuditError>;

/// Append-only in-memory audit ledger.
#[derive(Debug, Default)]
pub struct AuditLog {
    next_sequence: AtomicU64,
    events: DashMap<u64, AuditEvent>,
    last_hash: Mutex<Option<String>>,
}

impl AuditLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append(&self, input: AuditEventInput) -> AuditResult<AuditEvent> {
        let sequence = self.next_sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let timestamp = Utc::now();

        let mut guard = self
            .last_hash
            .lock()
            .map_err(|_| AuditError::LockPoisoned)?;
        let prev_hash = guard.clone();

        let hash = compute_event_hash(
            sequence,
            timestamp,
            &input.actor,
            &input.action,
            input.target.as_deref(),
            &input.metadata,
            prev_hash.as_deref(),
        );

        let event = AuditEvent {
            sequence,
            id: Uuid::new_v4(),
            timestamp,
            actor: input.actor,
            action: input.action,
            target: input.target,
            metadata: input.metadata,
            prev_hash,
            hash: hash.clone(),
        };

        self.events.insert(sequence, event.clone());
        *guard = Some(hash);

        Ok(event)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn list(&self) -> Vec<AuditEvent> {
        let mut out: Vec<AuditEvent> = self.events.iter().map(|e| e.value().clone()).collect();
        out.sort_by_key(|e| e.sequence);
        out
    }

    pub fn verify_chain(&self) -> AuditResult<()> {
        let events = self.list();
        let mut expected_prev: Option<String> = None;

        for event in events {
            if event.prev_hash != expected_prev {
                return Err(AuditError::ChainVerificationFailed {
                    sequence: event.sequence,
                });
            }

            let recomputed = compute_event_hash(
                event.sequence,
                event.timestamp,
                &event.actor,
                &event.action,
                event.target.as_deref(),
                &event.metadata,
                event.prev_hash.as_deref(),
            );

            if recomputed != event.hash {
                return Err(AuditError::ChainVerificationFailed {
                    sequence: event.sequence,
                });
            }

            expected_prev = Some(event.hash);
        }

        Ok(())
    }
}

fn compute_event_hash(
    sequence: u64,
    timestamp: DateTime<Utc>,
    actor: &str,
    action: &str,
    target: Option<&str>,
    metadata: &serde_json::Value,
    prev_hash: Option<&str>,
) -> String {
    let metadata_str = serde_json::to_string(metadata).unwrap_or_else(|_| "{}".to_string());
    let payload =
        format!("{sequence}|{timestamp}|{actor}|{action}|{target:?}|{metadata_str}|{prev_hash:?}");

    // Deterministic local hash for chain integrity checks.
    let mut hasher = DefaultHasher::new();
    payload.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
