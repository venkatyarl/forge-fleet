use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use thiserror::Error;
use uuid::Uuid;

use crate::{protocol::Operation, socket::PeerIdentity};

#[derive(Debug, Error)]
pub enum AuthorityError {
    #[error("authority mismatch")]
    Mismatch,
    #[error("nonce replay")]
    Replay,
    #[error("nonce expired")]
    Expired,
    #[error("database unavailable")]
    Database,
}

pub struct Issued {
    pub nonce: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct AuthorityStore {
    pool: PgPool,
    ttl_seconds: i64,
}

impl AuthorityStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            ttl_seconds: 120,
        }
    }

    pub async fn issue(
        &self,
        work_item_id: Uuid,
        repo_id: Uuid,
        operation: &Operation,
        request_digest: &str,
        peer: &PeerIdentity,
    ) -> Result<Issued, AuthorityError> {
        let nonce = random_nonce()?;
        let nonce_hash = hash(&nonce);
        let peer_digest = peer_digest(peer);
        let row = sqlx::query(
            r#"
            WITH authority AS (
              SELECT w.id work_item_id, w.repo_id, w.base_sha, l.id lease_id,
                     l.session_id, l.attempt, l.computer_id, s.slot,
                     wt.worktree_path, wt.base_branch, wt.task_branch, wt.head_sha
              FROM work_items w
              JOIN work_item_leases l ON l.work_item_id=w.id
                AND l.released_at IS NULL AND l.lease_state='building'
                AND l.lease_expires_at > clock_timestamp()
              JOIN sub_agents s ON s.id=l.sub_agent_id
                AND s.current_work_item_id=w.id AND s.status='busy'
              JOIN work_item_worktrees wt ON wt.work_item_id=w.id
                AND wt.sub_agent_id=s.id AND wt.computer_id=l.computer_id
                AND wt.status='active'
              WHERE w.id=$1 AND w.repo_id=$2 AND w.status='building'
              ORDER BY wt.created_at DESC LIMIT 1
            )
            INSERT INTO github_capability_nonces
              (nonce_hash, work_item_id, repo_id, lease_id, session_id, attempt,
               computer_id, slot, worktree_path, base_branch, task_ref, head_sha,
               operation, bound_value, request_digest, expected_remote_sha,
               peer_uid, peer_gid, peer_pid, peer_start_time, peer_executable_sha256,
               peer_cgroup_sha256, expires_at)
            SELECT $3, work_item_id, repo_id, lease_id, session_id, attempt,
                   computer_id, slot, worktree_path, base_branch, task_branch, head_sha,
                   $4, $5, $6, COALESCE(head_sha, repeat('0',40)),
                   $7, $8, $9, $10, $11, $12,
                   clock_timestamp() + make_interval(secs => $13)
            FROM authority
            RETURNING expires_at
            "#,
        )
        .bind(work_item_id)
        .bind(repo_id)
        .bind(nonce_hash)
        .bind(operation.name())
        .bind(operation.bound_value())
        .bind(request_digest)
        .bind(peer.uid as i64)
        .bind(peer.gid as i64)
        .bind(peer.pid)
        .bind(peer.start_time as i64)
        .bind(&peer.executable_sha256)
        .bind(peer_digest)
        .bind(self.ttl_seconds as i32)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AuthorityError::Database)?
        .ok_or(AuthorityError::Mismatch)?;
        Ok(Issued {
            nonce,
            expires_at: row.get("expires_at"),
        })
    }

    pub async fn consume_unavailable(
        &self,
        nonce: &str,
        work_item_id: Uuid,
        repo_id: Uuid,
        operation: &Operation,
        request_digest: &str,
        peer: &PeerIdentity,
    ) -> Result<(), AuthorityError> {
        let row = sqlx::query(
            r#"
            UPDATE github_capability_nonces n
               SET state='completed', consumed_at=clock_timestamp(),
                   completed_at=clock_timestamp(), outcome='backend_unavailable'
              FROM work_items w, work_item_leases l, sub_agents s, work_item_worktrees wt
             WHERE n.nonce_hash=$1 AND n.work_item_id=$2 AND n.repo_id=$3
               AND n.operation=$4 AND n.bound_value=$5 AND n.request_digest=$6
               AND n.peer_uid=$7 AND n.peer_gid=$8
               AND n.peer_pid=$9 AND n.peer_start_time=$10
               AND n.peer_executable_sha256=$11 AND n.peer_cgroup_sha256=$12
               AND n.state='issued' AND n.expires_at > clock_timestamp()
               AND w.id=n.work_item_id AND w.status='building' AND w.repo_id=n.repo_id
               AND l.id=n.lease_id AND l.work_item_id=w.id AND l.released_at IS NULL
               AND l.lease_state='building' AND l.lease_expires_at > clock_timestamp()
               AND l.session_id IS NOT DISTINCT FROM n.session_id
               AND l.attempt=n.attempt AND l.computer_id=n.computer_id
               AND s.id=l.sub_agent_id AND s.slot=n.slot AND s.status='busy'
               AND s.current_work_item_id=w.id
               AND wt.work_item_id=w.id AND wt.sub_agent_id=s.id
               AND wt.worktree_path=n.worktree_path AND wt.base_branch=n.base_branch
               AND wt.task_branch=n.task_ref AND wt.head_sha IS NOT DISTINCT FROM n.head_sha
               AND wt.status='active'
            RETURNING n.outcome
            "#,
        )
        .bind(hash(nonce))
        .bind(work_item_id)
        .bind(repo_id)
        .bind(operation.name())
        .bind(operation.bound_value())
        .bind(request_digest)
        .bind(peer.uid as i64)
        .bind(peer.gid as i64)
        .bind(peer.pid)
        .bind(peer.start_time as i64)
        .bind(&peer.executable_sha256)
        .bind(peer_digest(peer))
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AuthorityError::Database)?;
        if row.is_some() {
            return Ok(());
        }
        let state: Option<(String, DateTime<Utc>, Option<String>)> = sqlx::query_as(
            "SELECT state, expires_at, outcome FROM github_capability_nonces WHERE nonce_hash=$1",
        )
        .bind(hash(nonce))
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AuthorityError::Database)?;
        match state {
            Some((state, _, Some(outcome)))
                if state == "completed" && outcome == "backend_unavailable" =>
            {
                Ok(())
            }
            Some((_, expiry, _)) if expiry <= Utc::now() => Err(AuthorityError::Expired),
            Some(_) => Err(AuthorityError::Replay),
            None => Err(AuthorityError::Mismatch),
        }
    }
}

fn random_nonce() -> Result<String, AuthorityError> {
    let mut bytes = [0u8; 32];
    let mut file = std::fs::File::open("/dev/urandom").map_err(|_| AuthorityError::Database)?;
    use std::io::Read;
    file.read_exact(&mut bytes)
        .map_err(|_| AuthorityError::Database)?;
    Ok(hex::encode(bytes))
}

fn hash(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

fn peer_digest(peer: &PeerIdentity) -> Vec<u8> {
    hash(&peer.cgroup)
}
