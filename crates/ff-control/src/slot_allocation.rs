//! Per-project slot allocation.
//!
//! Splits the fleet's total sub-agent slots across the currently active
//! projects. By default every project carries the same weight and the split
//! is even; individual projects can be boosted or reduced via per-project
//! weight overrides in [`SlotAllocationConfig`].
//!
//! Allocation uses largest-remainder (Hamilton) apportionment: each project
//! first receives the floor of its exact proportional share, then the
//! leftover slots go to the projects with the largest fractional remainders.
//! Ties are broken by project name so the result is deterministic. When the
//! total weight is positive, the allocated slots always sum to `total_slots`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Slot allocation configuration — `[control.slot_allocation]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotAllocationConfig {
    /// Weight applied to any active project without an explicit override.
    #[serde(default = "default_project_weight")]
    pub default_weight: u32,

    /// Per-project weight overrides, keyed by project id.
    ///
    /// A weight of `0` excludes the project from slot allocation entirely.
    #[serde(default)]
    pub weights: BTreeMap<String, u32>,
}

impl SlotAllocationConfig {
    /// Return the effective weight for `project_id`.
    pub fn weight_for(&self, project_id: &str) -> u32 {
        self.weights
            .get(project_id)
            .copied()
            .unwrap_or(self.default_weight)
    }
}

impl Default for SlotAllocationConfig {
    fn default() -> Self {
        Self {
            default_weight: default_project_weight(),
            weights: BTreeMap::new(),
        }
    }
}

fn default_project_weight() -> u32 {
    1
}

/// Allocate `total_slots` across `active_projects` in proportion to their
/// configured weights.
///
/// Duplicate project ids are collapsed to a single entry. Every active
/// project appears in the result, including those allocated zero slots.
pub fn allocate_slots(
    active_projects: &[String],
    total_slots: usize,
    config: &SlotAllocationConfig,
) -> BTreeMap<String, usize> {
    let weights: BTreeMap<&str, u64> = active_projects
        .iter()
        .map(|p| (p.as_str(), u64::from(config.weight_for(p))))
        .collect();

    let total_weight: u64 = weights.values().sum();
    if total_weight == 0 {
        return weights.keys().map(|p| (p.to_string(), 0)).collect();
    }

    let total = total_slots as u64;
    let mut allocation: BTreeMap<String, usize> = BTreeMap::new();
    // (remainder, name) per project; larger remainders claim leftover slots.
    let mut remainders: Vec<(u64, &str)> = Vec::with_capacity(weights.len());
    let mut assigned: u64 = 0;

    for (project, weight) in &weights {
        let exact = total * weight;
        let share = exact / total_weight;
        assigned += share;
        allocation.insert(project.to_string(), share as usize);
        remainders.push((exact % total_weight, project));
    }

    // Largest fractional remainder first; equal remainders resolve by name.
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let mut leftover = total - assigned;
    for (remainder, project) in remainders {
        if leftover == 0 {
            break;
        }
        if remainder == 0 {
            // Exact shares consumed everything this project is entitled to.
            continue;
        }
        *allocation
            .get_mut(project)
            .expect("allocation seeded for every weighted project") += 1;
        leftover -= 1;
    }

    allocation
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projects(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn even_split_without_overrides() {
        let cfg = SlotAllocationConfig::default();
        let alloc = allocate_slots(&projects(&["alpha", "beta", "gamma"]), 9, &cfg);
        assert_eq!(alloc["alpha"], 3);
        assert_eq!(alloc["beta"], 3);
        assert_eq!(alloc["gamma"], 3);
    }

    #[test]
    fn remainder_slots_go_to_earliest_names_on_ties() {
        let cfg = SlotAllocationConfig::default();
        let alloc = allocate_slots(&projects(&["gamma", "alpha", "beta"]), 10, &cfg);
        assert_eq!(alloc.values().sum::<usize>(), 10);
        assert_eq!(alloc["alpha"], 4, "tie-break favors lexicographic order");
        assert_eq!(alloc["beta"], 3);
        assert_eq!(alloc["gamma"], 3);
    }

    #[test]
    fn weight_override_shifts_share() {
        let mut cfg = SlotAllocationConfig::default();
        cfg.weights.insert("alpha".to_string(), 3);
        let alloc = allocate_slots(&projects(&["alpha", "beta"]), 8, &cfg);
        assert_eq!(alloc["alpha"], 6);
        assert_eq!(alloc["beta"], 2);
    }

    #[test]
    fn zero_weight_excludes_project_but_keeps_it_listed() {
        let mut cfg = SlotAllocationConfig::default();
        cfg.weights.insert("paused".to_string(), 0);
        let alloc = allocate_slots(&projects(&["active", "paused"]), 4, &cfg);
        assert_eq!(alloc["active"], 4);
        assert_eq!(alloc["paused"], 0);
    }

    #[test]
    fn all_zero_weights_allocate_nothing() {
        let cfg = SlotAllocationConfig {
            default_weight: 0,
            weights: BTreeMap::new(),
        };
        let alloc = allocate_slots(&projects(&["alpha", "beta"]), 5, &cfg);
        assert_eq!(alloc["alpha"], 0);
        assert_eq!(alloc["beta"], 0);
    }

    #[test]
    fn duplicates_collapse_to_one_entry() {
        let cfg = SlotAllocationConfig::default();
        let alloc = allocate_slots(&projects(&["alpha", "alpha", "beta"]), 6, &cfg);
        assert_eq!(alloc.len(), 2);
        assert_eq!(alloc["alpha"], 3);
        assert_eq!(alloc["beta"], 3);
    }

    #[test]
    fn no_projects_yields_empty_allocation() {
        let cfg = SlotAllocationConfig::default();
        assert!(allocate_slots(&[], 8, &cfg).is_empty());
    }

    #[test]
    fn fewer_slots_than_projects_stays_within_total() {
        let cfg = SlotAllocationConfig::default();
        let alloc = allocate_slots(&projects(&["a", "b", "c", "d", "e"]), 2, &cfg);
        assert_eq!(alloc.values().sum::<usize>(), 2);
        assert!(alloc.values().all(|&s| s <= 1));
    }

    #[test]
    fn zero_slots_allocates_zero_everywhere() {
        let cfg = SlotAllocationConfig::default();
        let alloc = allocate_slots(&projects(&["alpha", "beta"]), 0, &cfg);
        assert_eq!(alloc["alpha"], 0);
        assert_eq!(alloc["beta"], 0);
    }

    #[test]
    fn weighted_allocation_sums_to_total() {
        let mut cfg = SlotAllocationConfig::default();
        cfg.weights.insert("alpha".to_string(), 5);
        cfg.weights.insert("beta".to_string(), 2);
        for total in 0..=17 {
            let alloc = allocate_slots(&projects(&["alpha", "beta", "gamma"]), total, &cfg);
            assert_eq!(alloc.values().sum::<usize>(), total, "total_slots={total}");
        }
    }

    #[test]
    fn weight_for_prefers_override_over_default() {
        let mut cfg = SlotAllocationConfig::default();
        cfg.weights.insert("alpha".to_string(), 7);
        assert_eq!(cfg.weight_for("alpha"), 7);
        assert_eq!(cfg.weight_for("beta"), 1);
    }

    #[test]
    fn config_deserializes_from_empty_object() {
        let cfg: SlotAllocationConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.default_weight, 1);
        assert!(cfg.weights.is_empty());
    }

    #[test]
    fn config_roundtrip_with_overrides() {
        let mut cfg = SlotAllocationConfig::default();
        cfg.default_weight = 2;
        cfg.weights.insert("alpha".to_string(), 4);
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: SlotAllocationConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.default_weight, 2);
        assert_eq!(parsed.weight_for("alpha"), 4);
        assert_eq!(parsed.weight_for("other"), 2);
    }
}
