//! Work queue metrics — queue size, processing time, and priority distribution.
//!
//! Prometheus metrics for the fleet work queues (e.g. the PM `work_items`
//! queue dispatched to sub-agents). Registration follows the same
//! `lazy_static` + `Once` pattern as [`crate::metrics`]; metrics are
//! registered with the shared [`crate::metrics::PROM_REGISTRY`] so they are
//! exported by the existing `/metrics` endpoint.

use std::sync::Once;

use lazy_static::lazy_static;
use prometheus::{HistogramOpts, HistogramVec, IntGaugeVec, Opts};

use crate::metrics::PROM_REGISTRY;

lazy_static! {
    // ── Work queue metrics ───────────────────────────────────────────

    /// Current number of pending items per queue (labels: queue).
    pub static ref WORK_QUEUE_SIZE: IntGaugeVec = IntGaugeVec::new(
        Opts::new("work_queue_size", "Current number of pending items in the work queue"),
        &["queue"],
    ).unwrap();

    /// Pending items broken down by priority band (labels: queue, priority).
    ///
    /// Priority labels are the coarse 1–5 bands used by `ff-mc` work items
    /// ("1" = critical … "5" = low).
    pub static ref WORK_QUEUE_SIZE_BY_PRIORITY: IntGaugeVec = IntGaugeVec::new(
        Opts::new(
            "work_queue_size_by_priority",
            "Pending work queue items by priority band (1=critical .. 5=low)",
        ),
        &["queue", "priority"],
    ).unwrap();

    /// Time spent processing a work item, in seconds (labels: queue).
    ///
    /// Buckets span quick shell tasks through long sub-agent builds.
    pub static ref WORK_QUEUE_PROCESSING_SECONDS: HistogramVec = HistogramVec::new(
        HistogramOpts::new(
            "work_queue_processing_seconds",
            "Work item processing time in seconds",
        )
        .buckets(vec![0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 1800.0, 3600.0]),
        &["queue"],
    ).unwrap();
}

static WORK_QUEUE_INIT: Once = Once::new();

/// Register the work queue metrics with the global registry.
///
/// Safe to call multiple times — only the first invocation registers.
/// Also invoked from [`crate::metrics::init_prometheus_metrics`].
pub fn init_work_queue_metrics() {
    WORK_QUEUE_INIT.call_once(|| {
        let r = &*PROM_REGISTRY;
        r.register(Box::new(WORK_QUEUE_SIZE.clone())).unwrap();
        r.register(Box::new(WORK_QUEUE_SIZE_BY_PRIORITY.clone()))
            .unwrap();
        r.register(Box::new(WORK_QUEUE_PROCESSING_SECONDS.clone()))
            .unwrap();
    });
}

/// Set the current pending size of a queue.
pub fn set_queue_size(queue: &str, size: i64) {
    WORK_QUEUE_SIZE.with_label_values(&[queue]).set(size);
}

/// Set the pending-item counts per priority band for a queue.
///
/// Bands absent from `counts` are reset to zero for priorities 1–5 so a
/// drained band doesn't keep reporting its last value.
pub fn set_priority_distribution(queue: &str, counts: &[(i32, i64)]) {
    for priority in 1..=5 {
        let count = counts
            .iter()
            .find(|(p, _)| *p == priority)
            .map(|(_, c)| *c)
            .unwrap_or(0);
        WORK_QUEUE_SIZE_BY_PRIORITY
            .with_label_values(&[queue, &priority.to_string()])
            .set(count);
    }
}

/// Record the processing time of one work item, in seconds.
pub fn observe_processing_time(queue: &str, duration_secs: f64) {
    WORK_QUEUE_PROCESSING_SECONDS
        .with_label_values(&[queue])
        .observe(duration_secs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_queue_size_gauge() {
        set_queue_size("test_size_queue", 7);
        assert_eq!(
            WORK_QUEUE_SIZE
                .with_label_values(&["test_size_queue"])
                .get(),
            7
        );
        set_queue_size("test_size_queue", 0);
        assert_eq!(
            WORK_QUEUE_SIZE
                .with_label_values(&["test_size_queue"])
                .get(),
            0
        );
    }

    #[test]
    fn test_priority_distribution_resets_absent_bands() {
        set_priority_distribution("test_prio_queue", &[(1, 3), (4, 2)]);
        let get = |p: &str| {
            WORK_QUEUE_SIZE_BY_PRIORITY
                .with_label_values(&["test_prio_queue", p])
                .get()
        };
        assert_eq!(get("1"), 3);
        assert_eq!(get("2"), 0);
        assert_eq!(get("4"), 2);

        // A band that drains must fall back to zero.
        set_priority_distribution("test_prio_queue", &[(4, 1)]);
        assert_eq!(get("1"), 0);
        assert_eq!(get("4"), 1);
    }

    #[test]
    fn test_processing_time_histogram() {
        observe_processing_time("test_hist_queue", 2.5);
        observe_processing_time("test_hist_queue", 120.0);
        let h = WORK_QUEUE_PROCESSING_SECONDS.with_label_values(&["test_hist_queue"]);
        assert_eq!(h.get_sample_count(), 2);
        assert!((h.get_sample_sum() - 122.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_init_is_idempotent() {
        init_work_queue_metrics();
        init_work_queue_metrics();
    }
}
