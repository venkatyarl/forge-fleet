//! Thread-safe in-memory cache of observed Pulse v2 beats.
//!
//! Readers (dashboard, election, router) feed each observed [`PulseBeatV2`]
//! into a shared [`PeerMap`] via [`PeerMap::update_from_beat`]. The map keeps
//! one [`PeerEntry`] per `computer_name` and lets callers query freshness
//! without re-parsing the full beat JSON every time.
//!
//! The map is cheap to clone (it holds an `Arc<RwLock<_>>`), so the usual
//! pattern is to clone it once per component and hand it around.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};

use crate::beat_v2::PulseBeatV2;

/// One computer's latest observed state, distilled from its most recent beat.
#[derive(Debug, Clone)]
pub struct PeerEntry {
    pub computer_name: String,
    pub last_beat_at: DateTime<Utc>,
    pub epoch: u64,
    pub role_claimed: String,
    /// Derived: `going_offline == false` AND `last_beat_at` is within 45s of
    /// the time the entry was last refreshed.
    pub is_healthy: bool,
    pub all_ips: Vec<String>,
    pub going_offline: bool,
}

/// Thread-safe map of `computer_name` → latest [`PeerEntry`].
#[derive(Clone, Default)]
pub struct PeerMap {
    inner: Arc<RwLock<HashMap<String, PeerEntry>>>,
}

impl PeerMap {
    /// Create an empty peer map.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Insert or replace the entry for `beat.computer_name`.
    ///
    /// Uses the beat's own `timestamp` as `last_beat_at` so callers can
    /// reconstruct freshness from the beat alone.
    pub fn update_from_beat(&self, beat: &PulseBeatV2) {
        let all_ips = beat
            .network
            .all_ips
            .iter()
            .map(|ip| ip.ip.clone())
            .collect::<Vec<_>>();

        let entry = PeerEntry {
            computer_name: beat.computer_name.clone(),
            last_beat_at: beat.timestamp,
            epoch: beat.epoch,
            role_claimed: beat.role_claimed.clone(),
            is_healthy: !beat.going_offline && Self::is_fresh(beat.timestamp, Utc::now(), 45),
            all_ips,
            going_offline: beat.going_offline,
        };

        if let Ok(mut guard) = self.inner.write() {
            guard.insert(beat.computer_name.clone(), entry);
        }
    }

    /// Mark `computer_name` as offline (sets `going_offline=true` and
    /// `is_healthy=false`). No-op if the peer is not in the map.
    pub fn mark_offline(&self, computer_name: &str) {
        if let Ok(mut guard) = self.inner.write() {
            if let Some(entry) = guard.get_mut(computer_name) {
                entry.going_offline = true;
                entry.is_healthy = false;
            }
        }
    }

    /// Clone and return one peer's entry, if present.
    pub fn get(&self, name: &str) -> Option<PeerEntry> {
        self.inner.read().ok()?.get(name).cloned()
    }

    /// Snapshot of all known peers.
    pub fn all(&self) -> Vec<PeerEntry> {
        self.inner
            .read()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Count peers whose `last_beat_at` is within `threshold_seconds` of
    /// `Utc::now()` and that are not going offline.
    pub fn count_alive(&self, threshold_seconds: i64) -> usize {
        let now = Utc::now();
        self.inner
            .read()
            .map(|g| {
                g.values()
                    .filter(|e| {
                        !e.going_offline && Self::is_fresh(e.last_beat_at, now, threshold_seconds)
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// Names of peers whose last beat is older than `threshold_seconds`
    /// relative to `Utc::now()` (or that are flagged `going_offline`).
    pub fn stale_names(&self, threshold_seconds: i64) -> Vec<String> {
        let now = Utc::now();
        self.stale_names_at(threshold_seconds, now)
    }

    /// Test hook: same as [`PeerMap::stale_names`] but with an explicit `now`.
    pub fn stale_names_at(&self, threshold_seconds: i64, now: DateTime<Utc>) -> Vec<String> {
        self.inner
            .read()
            .map(|g| {
                g.values()
                    .filter(|e| {
                        e.going_offline || !Self::is_fresh(e.last_beat_at, now, threshold_seconds)
                    })
                    .map(|e| e.computer_name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Test hook: [`PeerMap::count_alive`] with explicit `now`.
    pub fn count_alive_at(&self, threshold_seconds: i64, now: DateTime<Utc>) -> usize {
        self.inner
            .read()
            .map(|g| {
                g.values()
                    .filter(|e| {
                        !e.going_offline && Self::is_fresh(e.last_beat_at, now, threshold_seconds)
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    fn is_fresh(last: DateTime<Utc>, now: DateTime<Utc>, threshold_seconds: i64) -> bool {
        now.signed_duration_since(last) < Duration::seconds(threshold_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beat_v2::{Ip, PulseBeatV2};
    use std::sync::Arc;
    use std::thread;

    fn beat(name: &str, epoch: u64, ts: DateTime<Utc>, going_offline: bool) -> PulseBeatV2 {
        let mut b = PulseBeatV2::skeleton(name);
        b.timestamp = ts;
        b.epoch = epoch;
        b.going_offline = going_offline;
        b.network.primary_ip = "192.168.5.100".to_string();
        b.network.all_ips = vec![Ip {
            iface: "en0".into(),
            ip: "192.168.5.100".into(),
            kind: "v4".into(),
            paired_with: None,
            link_speed_gbps: None,
            medium: None,
        }];
        b
    }

    #[test]
    fn update_from_beat_replaces_entry() {
        let map = PeerMap::new();
        let now = Utc::now();
        map.update_from_beat(&beat("taylor", 1, now, false));
        assert_eq!(map.get("taylor").unwrap().epoch, 1);

        map.update_from_beat(&beat("taylor", 2, now, false));
        assert_eq!(map.get("taylor").unwrap().epoch, 2);
        assert_eq!(map.all().len(), 1);
    }

    #[test]
    fn mark_offline_sets_flags() {
        let map = PeerMap::new();
        map.update_from_beat(&beat("marcus", 1, Utc::now(), false));
        map.mark_offline("marcus");
        let e = map.get("marcus").unwrap();
        assert!(e.going_offline);
        assert!(!e.is_healthy);
    }

    #[test]
    fn concurrent_update_and_read_returns_latest() {
        let map = Arc::new(PeerMap::new());
        let writer_map = Arc::clone(&map);
        let reader_map = Arc::clone(&map);

        let writer = thread::spawn(move || {
            for epoch in 0..500u64 {
                let b = beat("sophie", epoch, Utc::now(), false);
                writer_map.update_from_beat(&b);
            }
        });

        let reader = thread::spawn(move || {
            let mut last_seen: u64 = 0;
            for _ in 0..500 {
                if let Some(e) = reader_map.get("sophie") {
                    // epoch must be monotonic from this writer
                    assert!(e.epoch >= last_seen);
                    last_seen = e.epoch;
                }
            }
            last_seen
        });

        writer.join().unwrap();
        let _ = reader.join().unwrap();

        // After writer is done, reader must see the final epoch.
        assert_eq!(map.get("sophie").unwrap().epoch, 499);
    }

    #[test]
    fn stale_names_filters_with_fake_now() {
        let map = PeerMap::new();
        let base = Utc::now();
        map.update_from_beat(&beat("fresh", 1, base, false));
        map.update_from_beat(&beat("old", 1, base - Duration::seconds(120), false));
        map.update_from_beat(&beat("leaving", 1, base, true));

        let stale = map.stale_names_at(45, base + Duration::seconds(1));
        assert!(stale.contains(&"old".to_string()));
        assert!(stale.contains(&"leaving".to_string()));
        assert!(!stale.contains(&"fresh".to_string()));
    }

    #[test]
    fn count_alive_correctness() {
        let map = PeerMap::new();
        let base = Utc::now();

        map.update_from_beat(&beat("a", 1, base, false));
        map.update_from_beat(&beat("b", 1, base - Duration::seconds(10), false));
        map.update_from_beat(&beat("c", 1, base - Duration::seconds(100), false));
        map.update_from_beat(&beat("d", 1, base, true)); // going offline

        assert_eq!(map.count_alive_at(45, base + Duration::seconds(1)), 2);
        assert_eq!(map.count_alive_at(200, base + Duration::seconds(1)), 3);
    }
}
