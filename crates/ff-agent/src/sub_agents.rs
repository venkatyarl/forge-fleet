//! Sub-agent slot manager.
//!
//! Each daemon host gets N concurrent worker slots (N =
//! `fleet_workers.sub_agent_count`), each with its own workspace directory
//! at `~/.forgefleet/sub-agents/sub-agent-{i}/` containing `scratch/`,
//! `checkpoints/`, and `cache/` subdirs.
//!
//! The defer-worker calls [`Slots::try_reserve`] before claiming a task.
//! The returned [`SlotGuard`] auto-releases on drop, so each concurrent
//! task gets a unique workspace for its duration.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const OS_HEADROOM_GB: f64 = 8.0;
const BYTES_PER_CONTEXT_TOKEN: f64 = 2048.0;

/// Capacity formula owned by the daemon. Resource divisions deliberately use
/// floors so a partially available slot never becomes schedulable.
pub fn compute_capacity_count(cores: u32, usable_ram_gb: f64, free_disk_gb: f64) -> u32 {
    let by_cores = cores / 3;
    let by_ram = (usable_ram_gb.max(0.0) / 6.0).floor() as u32;
    let by_disk = (free_disk_gb.max(0.0) / 20.0).floor() as u32;
    by_cores.min(by_ram).min(by_disk).clamp(1, 10)
}

/// Recompute this daemon's durable sub-agent rows from local hardware and
/// resident model deployments. Excess busy rows are intentionally untouched;
/// they become eligible for disabling on a later pass after their build ends.
pub async fn reconcile_capacity(pool: &sqlx::PgPool) -> Result<u32, String> {
    let worker_name = crate::fleet_info::resolve_this_worker_name().await;
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let total_ram_gb = local_total_ram_gb().unwrap_or(0.0);
    let free_disk_gb = local_free_disk_gb(&workspaces_root()).unwrap_or(0.0);

    let resident_gb: f64 = sqlx::query_scalar(
        r#"SELECT COALESCE(SUM(
                   COALESCE(lib.size_bytes, 0)::float8
                   + GREATEST(COALESCE(d.context_window, 0), 0)::float8
                     * GREATEST(COALESCE(d.parallel_slots, 1), 1)::float8 * $2
               ), 0) / 1e9
             FROM fleet_model_deployments d
             LEFT JOIN fleet_model_library lib ON lib.id = d.library_id
            WHERE LOWER(d.worker_name) = LOWER($1)
              AND d.desired_state = 'active'"#,
    )
    .bind(&worker_name)
    .bind(BYTES_PER_CONTEXT_TOKEN)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("resident model memory query: {e}"))?;

    let usable_ram_gb = (total_ram_gb - resident_gb - OS_HEADROOM_GB).max(0.0);
    let desired = compute_capacity_count(cores, usable_ram_gb, free_disk_gb);
    let workspace_root = workspaces_root().join("sub-agents");
    std::fs::create_dir_all(&workspace_root)
        .map_err(|e| format!("create {}: {e}", workspace_root.display()))?;

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let computer_id: sqlx::types::Uuid =
        sqlx::query_scalar("SELECT id FROM computers WHERE LOWER(name) = LOWER($1) LIMIT 1")
            .bind(&worker_name)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| format!("computer lookup for {worker_name}: {e}"))?;

    for slot in 0..desired as i32 {
        let workspace = workspace_root.join(format!("sub-agent-{slot}"));
        for sub in ["scratch", "checkpoints", "cache"] {
            std::fs::create_dir_all(workspace.join(sub))
                .map_err(|e| format!("create {}: {e}", workspace.display()))?;
        }
        sqlx::query(
            "INSERT INTO sub_agents (computer_id, slot, status, workspace_dir) \
             VALUES ($1, $2, 'idle', $3) \
             ON CONFLICT (computer_id, slot) DO UPDATE SET \
                 status = CASE WHEN sub_agents.status = 'disabled' THEN 'idle' \
                               ELSE sub_agents.status END, \
                 workspace_dir = EXCLUDED.workspace_dir",
        )
        .bind(computer_id)
        .bind(slot)
        .bind(workspace.to_string_lossy().as_ref())
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("upsert sub-agent slot {slot}: {e}"))?;
    }

    sqlx::query(
        "UPDATE sub_agents SET status = 'disabled' \
         WHERE computer_id = $1 AND slot >= $2 \
           AND status IN ('idle', 'error', 'disabled')",
    )
    .bind(computer_id)
    .bind(desired as i32)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("disable excess sub-agent slots: {e}"))?;

    sqlx::query(
        "UPDATE fleet_workers SET sub_agent_count = $1, updated_at = NOW() \
         WHERE LOWER(name) = LOWER($2)",
    )
    .bind(desired as i32)
    .bind(&worker_name)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("update fleet worker capacity: {e}"))?;
    tx.commit().await.map_err(|e| e.to_string())?;

    tracing::info!(
        worker = %worker_name,
        desired,
        cores,
        total_ram_gb,
        resident_gb,
        usable_ram_gb,
        free_disk_gb,
        "reconciled sub-agent capacity"
    );
    Ok(desired)
}

fn local_total_ram_gb() -> Option<f64> {
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        let bytes: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()?;
        return Some(bytes as f64 / 1e9);
    }
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: f64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb / 1_000_000.0)
}

fn local_free_disk_gb(path: &Path) -> Option<f64> {
    let output = std::process::Command::new("df")
        .args(["-Pk", path.to_str()?])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let available_kb: f64 = String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()?;
    Some(available_kb / 1_000_000.0)
}

/// Compute the default sub-agent slot count for a host based on its
/// hardware. Formula: `max(1, min(cores/2, ram_gb/16, cap))` where `cap`
/// = 8 if the host has an NVIDIA GPU AND ram_gb >= 64, else 4.
pub fn compute_default_count(cores: u32, ram_gb: u32, has_nvidia_gpu: bool) -> u32 {
    let cap: u32 = if has_nvidia_gpu && ram_gb >= 64 { 8 } else { 4 };
    let by_cores = cores / 2;
    let by_ram = ram_gb / 16;
    let candidate = by_cores.min(by_ram).min(cap);
    candidate.max(1)
}

/// Returns the root directory (~/.forgefleet) for sub-agent workspaces.
fn workspaces_root() -> PathBuf {
    if let Some(h) = home_dir_xplat() {
        h.join(".forgefleet")
    } else if cfg!(windows) {
        PathBuf::from(r"C:\ProgramData\forgefleet")
    } else {
        PathBuf::from("/tmp/.forgefleet")
    }
}

fn home_dir_xplat() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        return Some(PathBuf::from(h));
    }
    if let Ok(h) = std::env::var("USERPROFILE") {
        return Some(PathBuf::from(h));
    }
    None
}

/// Ensure `~/.forgefleet/sub-agents/sub-agent-0 .. sub-agent-{count-1}/` exist with
/// `scratch/`, `checkpoints/`, and `cache/` subdirs. Idempotent — no
/// error if they already exist. Returns the workspace paths in index
/// order.
pub fn ensure_workspaces(count: u32) -> Result<Vec<PathBuf>, String> {
    let root = workspaces_root();
    let parent = root.join("sub-agents");
    std::fs::create_dir_all(&parent).map_err(|e| format!("create {}: {e}", parent.display()))?;

    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let ws = parent.join(format!("sub-agent-{i}"));
        for sub in ["scratch", "checkpoints", "cache"] {
            let p = ws.join(sub);
            std::fs::create_dir_all(&p).map_err(|e| format!("create {}: {e}", p.display()))?;
        }
        out.push(ws);
    }
    Ok(out)
}

#[derive(Debug)]
struct SlotsInner {
    /// `in_use[i] == true` means slot i is reserved.
    in_use: Vec<bool>,
    /// Workspace path for each slot (same length as `in_use`).
    workspaces: Vec<PathBuf>,
}

/// Thread-safe pool of sub-agent slots. Clones share state.
#[derive(Clone, Debug)]
pub struct Slots {
    inner: Arc<Mutex<SlotsInner>>,
}

impl Slots {
    /// Create a new pool with `count` slots. Workspaces are created on
    /// disk; panics only on Mutex poison.
    pub fn new(count: u32) -> Self {
        let workspaces = ensure_workspaces(count).unwrap_or_else(|e| {
            eprintln!("sub_agents: ensure_workspaces({count}) failed: {e}");
            let parent = workspaces_root().join("sub-agents");
            (0..count)
                .map(|i| parent.join(format!("sub-agent-{i}")))
                .collect()
        });
        Self {
            inner: Arc::new(Mutex::new(SlotsInner {
                in_use: vec![false; count as usize],
                workspaces,
            })),
        }
    }

    /// Live-scale the slot count. Growing creates new workspaces.
    /// Shrinking is lazy: if all excess slots are idle they are dropped
    /// immediately; otherwise the surplus is marked for trim on next
    /// release (any still-in-use slot above the new limit will simply
    /// remove its entry when released).
    pub fn set_count(&self, count: u32) {
        let mut inner = self.inner.lock().unwrap();
        let cur = inner.in_use.len() as u32;
        if count == cur {
            return;
        }
        if count > cur {
            // Grow.
            if let Err(e) = ensure_workspaces(count) {
                eprintln!("sub_agents: ensure_workspaces({count}) failed: {e}");
            }
            let parent = workspaces_root().join("sub-agents");
            for i in cur..count {
                inner.in_use.push(false);
                inner.workspaces.push(parent.join(format!("sub-agent-{i}")));
            }
        } else {
            // Shrink: drop trailing idle slots. In-use slots stay until
            // released, then get trimmed by `release_slot`.
            while (inner.in_use.len() as u32) > count {
                let last = inner.in_use.len() - 1;
                if inner.in_use[last] {
                    break; // stop; still in use
                }
                inner.in_use.pop();
                inner.workspaces.pop();
            }
        }
    }

    /// Try to reserve a free slot. Returns `None` if all slots are
    /// busy.
    pub fn try_reserve(&self) -> Option<SlotGuard<'_>> {
        let mut inner = self.inner.lock().unwrap();
        for i in 0..inner.in_use.len() {
            if !inner.in_use[i] {
                inner.in_use[i] = true;
                let workspace = inner.workspaces[i].clone();
                return Some(SlotGuard {
                    slots: self,
                    index: i as u32,
                    workspace,
                });
            }
        }
        None
    }

    /// Total slot count.
    pub fn capacity(&self) -> u32 {
        self.inner.lock().unwrap().in_use.len() as u32
    }

    /// Number of slots currently reserved.
    pub fn in_use(&self) -> u32 {
        self.inner
            .lock()
            .unwrap()
            .in_use
            .iter()
            .filter(|b| **b)
            .count() as u32
    }

    fn release(&self, index: u32) {
        let mut inner = self.inner.lock().unwrap();
        let idx = index as usize;
        if idx < inner.in_use.len() {
            inner.in_use[idx] = false;
        }
        // Trim any trailing idle slots that were left over from a
        // shrink-while-busy.
        // NOTE: we intentionally do NOT auto-trim here because the vec
        // length is the committed count.  set_count() handles trimming
        // when it shrinks below the current length.
    }
}

/// Auto-releasing handle for a reserved slot.
pub struct SlotGuard<'a> {
    slots: &'a Slots,
    index: u32,
    workspace: PathBuf,
}

impl SlotGuard<'_> {
    /// Slot index (0-based).
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Workspace directory for this slot.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.slots.release(self.index);
    }
}

/// Owned variant of [`SlotGuard`] that holds an `Arc<Slots>` rather than
/// a borrow. Useful when passing the guard into a `tokio::spawn`'d
/// task where a borrowed lifetime wouldn't satisfy `'static`.
pub struct OwnedSlotGuard {
    slots: Slots,
    index: u32,
    workspace: PathBuf,
    released: bool,
}

impl OwnedSlotGuard {
    pub fn index(&self) -> u32 {
        self.index
    }
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

impl Drop for OwnedSlotGuard {
    fn drop(&mut self) {
        if !self.released {
            self.slots.release(self.index);
        }
    }
}

impl Slots {
    /// Like [`try_reserve`] but returns an owned guard (`'static`),
    /// suitable for moving into a spawned task.
    pub fn try_reserve_owned(&self) -> Option<OwnedSlotGuard> {
        let g = self.try_reserve()?;
        let index = g.index;
        let workspace = g.workspace.clone();
        // Forget the borrowed guard so its Drop doesn't release — the
        // OwnedSlotGuard takes over responsibility.
        std::mem::forget(g);
        Some(OwnedSlotGuard {
            slots: self.clone(),
            index,
            workspace,
            released: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formula_basic() {
        // 8 cores, 32 GB, no gpu -> min(4, 2, 4) = 2
        assert_eq!(compute_default_count(8, 32, false), 2);
        // 32 cores, 128 GB, gpu -> min(16, 8, 8) = 8
        assert_eq!(compute_default_count(32, 128, true), 8);
        // Tiny box: 2 cores, 4 GB -> max(1, min(1, 0, 4)) = 1
        assert_eq!(compute_default_count(2, 4, false), 1);
        // NVIDIA but low RAM uses cap=4
        assert_eq!(compute_default_count(64, 32, true), 2);
    }

    #[test]
    fn capacity_formula_uses_tightest_resource_and_clamps() {
        assert_eq!(compute_capacity_count(32, 120.0, 500.0), 10);
        assert_eq!(compute_capacity_count(32, 30.0, 500.0), 5);
        assert_eq!(compute_capacity_count(32, 120.0, 79.9), 3);
        assert_eq!(compute_capacity_count(1, 0.0, 0.0), 1);
    }

    #[test]
    fn reserve_and_release() {
        let s = Slots::new(2);
        assert_eq!(s.capacity(), 2);
        assert_eq!(s.in_use(), 0);
        let a = s.try_reserve().unwrap();
        let b = s.try_reserve().unwrap();
        assert_eq!(s.in_use(), 2);
        assert!(s.try_reserve().is_none());
        assert_ne!(a.index(), b.index());
        drop(a);
        assert_eq!(s.in_use(), 1);
        let c = s.try_reserve().unwrap();
        assert_eq!(s.in_use(), 2);
        drop(b);
        drop(c);
        assert_eq!(s.in_use(), 0);
    }

    #[test]
    fn set_count_grows_and_shrinks() {
        let s = Slots::new(1);
        s.set_count(3);
        assert_eq!(s.capacity(), 3);
        s.set_count(1);
        assert_eq!(s.capacity(), 1);
    }
}
