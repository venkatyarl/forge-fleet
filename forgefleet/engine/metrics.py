"""Metrics Export — Prometheus/Grafana compatible metrics.

Item #14: Pretty dashboards via Grafana.
Exports metrics in Prometheus text format on /metrics endpoint.
"""
import time
from dataclasses import dataclass, field


@dataclass
class MetricsCollector:
    """Collect and export fleet metrics in Prometheus format.
    
    Metrics:
    - forgefleet_endpoints_total{tier}
    - forgefleet_endpoints_healthy{tier}
    - forgefleet_endpoints_busy{tier}
    - forgefleet_tasks_total{status}
    - forgefleet_tasks_duration_seconds{agent}
    - forgefleet_tokens_total{model}
    - forgefleet_cost_savings_usd
    - forgefleet_learnings_total{outcome}
    - forgefleet_scheduler_state
    """
    _counters: dict = field(default_factory=dict)
    _gauges: dict = field(default_factory=dict)
    _histograms: dict = field(default_factory=dict)
    
    def set_gauge(self, name: str, value: float, labels: dict = None):
        """Set a gauge metric."""
        key = self._make_key(name, labels)
        self._gauges[key] = (name, value, labels or {})
    
    def inc_counter(self, name: str, labels: dict = None, amount: float = 1):
        """Increment a counter metric."""
        key = self._make_key(name, labels)
        if key not in self._counters:
            self._counters[key] = (name, 0, labels or {})
        _, current, lbl = self._counters[key]
        self._counters[key] = (name, current + amount, lbl)
    
    def observe(self, name: str, value: float, labels: dict = None):
        """Record a histogram observation."""
        key = self._make_key(name, labels)
        if key not in self._histograms:
            self._histograms[key] = (name, [], labels or {})
        _, values, lbl = self._histograms[key]
        values.append(value)
        # Keep last 1000 observations
        if len(values) > 1000:
            self._histograms[key] = (name, values[-500:], lbl)
    
    def _make_key(self, name: str, labels: dict = None) -> str:
        if not labels:
            return name
        label_str = ",".join(f'{k}="{v}"' for k, v in sorted((labels or {}).items()))
        return f"{name}{{{label_str}}}"
    
    def export_prometheus(self) -> str:
        """Export all metrics in Prometheus text format."""
        lines = []
        
        # Gauges
        for key, (name, value, labels) in self._gauges.items():
            label_str = self._format_labels(labels)
            lines.append(f"# TYPE {name} gauge")
            lines.append(f"{name}{label_str} {value}")
        
        # Counters
        for key, (name, value, labels) in self._counters.items():
            label_str = self._format_labels(labels)
            lines.append(f"# TYPE {name} counter")
            lines.append(f"{name}{label_str} {value}")
        
        # Histograms (simplified — just sum and count)
        for key, (name, values, labels) in self._histograms.items():
            if not values:
                continue
            label_str = self._format_labels(labels)
            lines.append(f"# TYPE {name} summary")
            lines.append(f"{name}_count{label_str} {len(values)}")
            lines.append(f"{name}_sum{label_str} {sum(values):.2f}")
        
        return "\n".join(lines) + "\n"
    
    def _format_labels(self, labels: dict) -> str:
        if not labels:
            return ""
        parts = [f'{k}="{v}"' for k, v in sorted(labels.items())]
        return "{" + ",".join(parts) + "}"
    
    def update_fleet_metrics(self, endpoints: list, tickets: dict = None):
        """Update fleet-wide metrics from discovery results."""
        by_tier = {}
        for ep in endpoints:
            t = getattr(ep, "tier", 0)
            by_tier.setdefault(t, {"total": 0, "healthy": 0, "busy": 0})
            by_tier[t]["total"] += 1
            if getattr(ep, "healthy", False):
                by_tier[t]["healthy"] += 1
            if getattr(ep, "slots_busy", 0) > 0:
                by_tier[t]["busy"] += 1
        
        for tier, stats in by_tier.items():
            labels = {"tier": str(tier)}
            self.set_gauge("forgefleet_endpoints_total", stats["total"], labels)
            self.set_gauge("forgefleet_endpoints_healthy", stats["healthy"], labels)
            self.set_gauge("forgefleet_endpoints_busy", stats["busy"], labels)
        
        if tickets:
            for status, count in tickets.items():
                self.set_gauge("forgefleet_tickets", count, {"status": status})


# Global metrics instance
_metrics = MetricsCollector()

def get_metrics() -> MetricsCollector:
    return _metrics
