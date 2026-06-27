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
        // Hold the hash-chain lock across the ENTIRE sequence+hash allocation so
        // that sequence numbers are assigned in the same order the chain is
        // linked. Allocating the sequence before taking the lock let a
        // later-sequenced append win the lock first, producing a chain where
        // event N carries event N+1's hash as its `prev_hash` — which then
        // fails its own `verify_chain` even though no tampering occurred.
        let mut guard = self
            .last_hash
            .lock()
            .map_err(|_| AuditError::LockPoisoned)?;

        let sequence = self.next_sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let timestamp = Utc::now();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;

    fn input(actor: &str) -> AuditEventInput {
        AuditEventInput {
            actor: actor.to_string(),
            action: "test".to_string(),
            target: None,
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn sequential_append_chains_and_verifies() {
        let log = AuditLog::new();
        let a = log.append(input("a")).unwrap();
        let b = log.append(input("b")).unwrap();
        let c = log.append(input("c")).unwrap();

        assert_eq!((a.sequence, b.sequence, c.sequence), (1, 2, 3));
        assert_eq!(a.prev_hash, None);
        assert_eq!(b.prev_hash.as_deref(), Some(a.hash.as_str()));
        assert_eq!(c.prev_hash.as_deref(), Some(b.hash.as_str()));
        log.verify_chain().unwrap();
    }

    #[test]
    fn verify_chain_detects_tampering() {
        let log = AuditLog::new();
        log.append(input("a")).unwrap();
        log.append(input("b")).unwrap();

        // Corrupt a stored event's metadata without re-linking the chain.
        if let Some(mut entry) = log.events.get_mut(&1) {
            entry.metadata = serde_json::json!({"tampered": true});
        }

        assert!(matches!(
            log.verify_chain(),
            Err(AuditError::ChainVerificationFailed { sequence: 1 })
        ));
    }

    // Regression guard for the seq/hash-chain race: when the sequence was
    // allocated OUTSIDE the `last_hash` mutex, concurrent appends could link
    // the chain in a different order than the sequence numbers, so the ledger
    // failed its own `verify_chain`. With allocation moved under the lock this
    // is always a valid, contiguous chain regardless of interleaving.
    #[test]
    fn concurrent_append_preserves_chain_integrity() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 64;

        let log = Arc::new(AuditLog::new());
        let barrier = Arc::new(Barrier::new(THREADS));

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let log = Arc::clone(&log);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for i in 0..PER_THREAD {
                        log.append(input(&format!("actor-{t}-{i}"))).unwrap();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let events = log.list();
        assert_eq!(events.len(), THREADS * PER_THREAD);

        // Sequences must be exactly 1..=N with no gaps or duplicates.
        for (idx, event) in events.iter().enumerate() {
            assert_eq!(event.sequence, idx as u64 + 1);
        }

        // The chain must validate end-to-end.
        log.verify_chain().unwrap();
    }
}
