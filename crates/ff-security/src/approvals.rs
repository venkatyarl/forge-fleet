use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: Uuid,
    pub requester: String,
    pub action: String,
    pub reason: Option<String>,
    pub status: ApprovalStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by: Option<String>,
    pub decision_reason: Option<String>,
}

impl ApprovalRequest {
    pub fn is_pending(&self) -> bool {
        self.status == ApprovalStatus::Pending
    }
}

#[derive(Debug, Error)]
pub enum ApprovalError {
    #[error("approval request not found: {0}")]
    NotFound(Uuid),

    #[error("approval request already finalized with status: {0:?}")]
    AlreadyFinalized(ApprovalStatus),

    #[error("approval request expired")]
    Expired,

    #[error("invalid approval TTL (must be > 0 seconds)")]
    InvalidTtl,
}

pub type ApprovalResult<T> = std::result::Result<T, ApprovalError>;

/// In-memory approval state store.
#[derive(Debug, Default)]
pub struct ApprovalManager {
    requests: DashMap<Uuid, ApprovalRequest>,
}

impl ApprovalManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request(
        &self,
        requester: impl Into<String>,
        action: impl Into<String>,
        reason: Option<String>,
        ttl: Duration,
    ) -> ApprovalResult<ApprovalRequest> {
        if ttl <= Duration::zero() {
            return Err(ApprovalError::InvalidTtl);
        }

        let created_at = Utc::now();
        let approval = ApprovalRequest {
            id: Uuid::new_v4(),
            requester: requester.into(),
            action: action.into(),
            reason,
            status: ApprovalStatus::Pending,
            created_at,
            expires_at: created_at + ttl,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
        };

        self.requests.insert(approval.id, approval.clone());
        debug!(approval_id = %approval.id, "approval requested");
        Ok(approval)
    }

    pub fn approve(
        &self,
        id: Uuid,
        approver: impl Into<String>,
        decision_reason: Option<String>,
    ) -> ApprovalResult<ApprovalRequest> {
        self.finalize(
            id,
            ApprovalStatus::Approved,
            approver.into(),
            decision_reason,
        )
    }

    pub fn deny(
        &self,
        id: Uuid,
        approver: impl Into<String>,
        decision_reason: Option<String>,
    ) -> ApprovalResult<ApprovalRequest> {
        self.finalize(id, ApprovalStatus::Denied, approver.into(), decision_reason)
    }

    pub fn expire_stale(&self, now: DateTime<Utc>) -> usize {
        let mut expired = 0usize;
        for mut entry in self.requests.iter_mut() {
            let req = entry.value_mut();
            if req.status == ApprovalStatus::Pending && req.expires_at <= now {
                req.status = ApprovalStatus::Expired;
                req.decided_at = Some(now);
                req.decided_by = Some("system".to_string());
                req.decision_reason = Some("approval expired due to timeout".to_string());
                expired += 1;
            }
        }

        if expired > 0 {
            warn!(expired_count = expired, "expired stale approvals");
        }

        expired
    }

    pub fn get(&self, id: Uuid) -> Option<ApprovalRequest> {
        self.requests.get(&id).map(|entry| entry.value().clone())
    }

    fn finalize(
        &self,
        id: Uuid,
        new_status: ApprovalStatus,
        approver: String,
        decision_reason: Option<String>,
    ) -> ApprovalResult<ApprovalRequest> {
        let now = Utc::now();
        let mut entry = self
            .requests
            .get_mut(&id)
            .ok_or(ApprovalError::NotFound(id))?;

        let req = entry.value_mut();
        if req.status != ApprovalStatus::Pending {
            return Err(ApprovalError::AlreadyFinalized(req.status.clone()));
        }

        if req.expires_at <= now {
            req.status = ApprovalStatus::Expired;
            req.decided_at = Some(now);
            req.decided_by = Some("system".to_string());
            req.decision_reason = Some("approval expired before decision".to_string());
            return Err(ApprovalError::Expired);
        }

        req.status = new_status;
        req.decided_at = Some(now);
        req.decided_by = Some(approver);
        req.decision_reason = decision_reason;

        Ok(req.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_pending_request() {
        let mgr = ApprovalManager::new();
        let req = mgr
            .request(
                "user-1",
                "run elevated command",
                Some("need root to install runtime".into()),
                Duration::minutes(10),
            )
            .expect("request should be created");

        let approved = mgr
            .approve(
                req.id,
                "admin-1",
                Some("approved for maintenance window".into()),
            )
            .expect("approval should succeed");

        assert_eq!(approved.status, ApprovalStatus::Approved);
        assert_eq!(approved.decided_by.as_deref(), Some("admin-1"));
    }

    #[test]
    fn cannot_approve_after_denial() {
        let mgr = ApprovalManager::new();
        let req = mgr
            .request("user-1", "delete /tmp", None, Duration::minutes(5))
            .expect("request should be created");

        mgr.deny(req.id, "admin-1", Some("not safe".into()))
            .expect("deny should succeed");

        let err = mgr
            .approve(req.id, "admin-2", None)
            .expect_err("second decision should fail");

        match err {
            ApprovalError::AlreadyFinalized(status) => {
                assert_eq!(status, ApprovalStatus::Denied);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn expire_stale_pending_requests() {
        let mgr = ApprovalManager::new();
        let req = mgr
            .request("user-1", "dangerous op", None, Duration::seconds(1))
            .expect("request should be created");

        let expired = mgr.expire_stale(Utc::now() + Duration::seconds(5));
        assert_eq!(expired, 1);

        let stored = mgr.get(req.id).expect("request should exist");
        assert_eq!(stored.status, ApprovalStatus::Expired);
    }
}
