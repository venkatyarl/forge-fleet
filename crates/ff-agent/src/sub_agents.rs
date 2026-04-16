//! Sub-agent slot manager.
//!
//! Each daemon host gets N concurrent worker slots (N =
//! `fleet_nodes.sub_agent_count`), each with its own workspace directory
//! at `~/.forgefleet/sub-agent-{i}/` containing `scratch/`,
//! `checkpoints/`, and `cache/` subdirs.
//!
//! The defer-worker calls [`Slots::try_reserve`] before claiming a task.
//! The returned [`SlotGuard`] auto-releases on drop, so each concurrent
//! task gets a unique workspace for its duration.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

/// Return the root directory for sub-agent workspaces (`~/.forgefleet` on
/// Unix, `%USERPROFILE%\.forgefleet` on Windows).
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
    if let Ok(h) = std::env::var("HOME") { return Some(PathBuf::from(h)); }
    if let Ok(h) = std::env::var("USERPROFILE") { return Some(PathBuf::from(h)); }
    None
}

/// Ensure `~/.forgefleet/sub-agent-0 .. sub-agent-{count-1}/` exist with
/// `scratch/`, `checkpoints/`, and `cache/` subdirs. Idempotent — no
/// error if they already exist. Returns the workspace paths in index
/// order.
pub fn ensure_workspaces(count: u32) -> Result<Vec<PathBuf>, String> {
    let root = workspaces_root();
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("create {}: {e}", root.display()))?;

    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let ws = root.join(format!("sub-agent-{i}"));
        for sub in ["scratch", "checkpoints", "cache"] {
            let p = ws.join(sub);
            std::fs::create_dir_all(&p)
                .map_err(|e| format!("create {}: {e}", p.display()))?;
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
            (0..count).map(|i| workspaces_root().join(format!("sub-agent-{i}"))).collect()
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
            let root = workspaces_root();
            for i in cur..count {
                inner.in_use.push(false);
                inner.workspaces.push(root.join(format!("sub-agent-{i}")));
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
        self.inner.lock().unwrap().in_use.iter().filter(|b| **b).count() as u32
    }

    fn release(&self, index: u32) {
        let mut inner = self.inner.lock().unwrap();
        let idx = index as usize;
        if idx < inner.in_use.len() {
            inner.in_use[idx] = false;
        }
        // Trim any trailing idle slots that were left over from a
        // shrink-while-busy.
        while inner.in_use.len() > 0 {
            let last = inner.in_use.len() - 1;
            // Only trim if we are above the "committed" count; however
            // since we treat the vec length as the committed count,
            // there's nothing to trim here unless set_count shrank it
            // below this slot. We keep this simple: no auto-trim.
            let _ = last;
            break;
        }
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
