//! Process-local cache of the leader-election result.
//!
//! `leader_tick` is the only subsystem that should have to read/write
//! `fleet_leader_state` for routine leadership decisions. Leader-gated runtime
//! ticks use the atomic bit here so a skip path is a local memory read instead
//! of a Postgres query.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use ff_db::leader_state::LeaderState;
use tokio::sync::RwLock;
use uuid::Uuid;

pub type SharedLeaderCache = Arc<LeaderCache>;

static GLOBAL_LEADER_CACHE: OnceLock<SharedLeaderCache> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderInfo {
    pub computer_id: Option<Uuid>,
    pub member_name: Option<String>,
    pub epoch: Option<i64>,
    pub elected_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
}

impl Default for LeaderInfo {
    fn default() -> Self {
        Self {
            computer_id: None,
            member_name: None,
            epoch: None,
            elected_at: None,
            reason: None,
            heartbeat_at: None,
            observed_at: Utc::now(),
        }
    }
}

impl From<&LeaderState> for LeaderInfo {
    fn from(state: &LeaderState) -> Self {
        Self {
            computer_id: Some(state.computer_id),
            member_name: Some(state.member_name.clone()),
            epoch: Some(state.epoch),
            elected_at: Some(state.elected_at.clone()),
            reason: state.reason.clone(),
            heartbeat_at: Some(state.heartbeat_at.clone()),
            observed_at: Utc::now(),
        }
    }
}

#[derive(Debug)]
pub struct LeaderCache {
    is_current_leader: AtomicBool,
    info: RwLock<LeaderInfo>,
}

impl LeaderCache {
    pub fn new() -> Self {
        Self {
            is_current_leader: AtomicBool::new(false),
            info: RwLock::new(LeaderInfo::default()),
        }
    }

    pub fn shared() -> SharedLeaderCache {
        Arc::new(Self::new())
    }

    pub fn global() -> SharedLeaderCache {
        GLOBAL_LEADER_CACHE.get_or_init(Self::shared).clone()
    }

    pub async fn update_state(&self, is_current_leader: bool, info: LeaderInfo) {
        self.is_current_leader
            .store(is_current_leader, Ordering::Relaxed);
        *self.info.write().await = info;
    }

    pub fn is_current_leader(&self) -> bool {
        self.is_current_leader.load(Ordering::Relaxed)
    }

    pub async fn leader_info(&self) -> LeaderInfo {
        self.info.read().await.clone()
    }
}

impl Default for LeaderCache {
    fn default() -> Self {
        Self::new()
    }
}

pub fn is_current_leader() -> bool {
    GLOBAL_LEADER_CACHE
        .get_or_init(LeaderCache::shared)
        .is_current_leader()
}
