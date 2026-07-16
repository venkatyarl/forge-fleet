"""Cost Tracker — track what we'd be paying if using API models.

Item #9: Even though our fleet is free, tracking costs shows ROI.
"This month ForgeFleet did $4,200 worth of API calls for $0."
"""
import json
import os
import time
from dataclasses import dataclass, field


# API pricing (per 1M tokens, as of March 2026)
API_PRICING = {
    # Input / Output per 1M tokens
    "gpt-4o": {"input": 2.50, "output": 10.00},
    "gpt-4-turbo": {"input": 10.00, "output": 30.00},
    "claude-3.5-sonnet": {"input": 3.00, "output": 15.00},
    "claude-opus-4": {"input": 15.00, "output": 75.00},
    "codex": {"input": 3.00, "output": 15.00},
    # What our local models would cost if we used equivalent APIs
    "qwen3.5-9b": {"equivalent": "gpt-4o-mini", "input": 0.15, "output": 0.60},
    "qwen2.5-coder-32b": {"equivalent": "codex", "input": 3.00, "output": 15.00},
    "qwen2.5-72b": {"equivalent": "claude-3.5-sonnet", "input": 3.00, "output": 15.00},
    "qwen3-235b": {"equivalent": "claude-opus-4", "input": 15.00, "output": 75.00},
}


@dataclass
class UsageRecord:
    """A single API usage record."""
    model: str
    input_tokens: int
    output_tokens: int
    cost_if_api: float
    timestamp: float
    task_type: str = ""


class CostTracker:
    """Track token usage and equivalent API costs.
    
    Every LLM call gets logged. Monthly reports show how much
    we'd be paying if using cloud APIs instead of local models.
    """
    
    def __init__(self, db_path: str = ""):
        if not db_path:
            db_dir = os.path.expanduser("~/.forgefleet")
            os.makedirs(db_dir, exist_ok=True)
            db_path = os.path.join(db_dir, "costs.json")
        self.db_path = db_path
        self.records: list[UsageRecord] = []
        self._load()
    
    def _load(self):
        if os.path.exists(self.db_path):
            try:
                with open(self.db_path) as f:
                    data = json.load(f)
                for d in data:
                    self.records.append(UsageRecord(**d))
            except Exception:
                pass
    
    def _save(self):
        data = [
            {"model": r.model, "input_tokens": r.input_tokens,
             "output_tokens": r.output_tokens, "cost_if_api": r.cost_if_api,
             "timestamp": r.timestamp, "task_type": r.task_type}
            for r in self.records[-10000:]  # Keep last 10k records
        ]
        with open(self.db_path, "w") as f:
            json.dump(data, f)
    
    def record(self, model: str, input_tokens: int, output_tokens: int,
               task_type: str = ""):
        """Record a usage event."""
        cost = self._estimate_cost(model, input_tokens, output_tokens)
        self.records.append(UsageRecord(
            model=model, input_tokens=input_tokens,
            output_tokens=output_tokens, cost_if_api=cost,
            timestamp=time.time(), task_type=task_type,
        ))
        
        if len(self.records) % 100 == 0:
            self._save()
    
    def _estimate_cost(self, model: str, input_tokens: int, output_tokens: int) -> float:
        """Estimate what this would cost via API."""
        model_lower = model.lower()
        
        pricing = None
        for key, p in API_PRICING.items():
            if key in model_lower:
                pricing = p
                break
        
        if not pricing:
            # Default to mid-tier pricing
            pricing = {"input": 3.00, "output": 15.00}
        
        input_cost = (input_tokens / 1_000_000) * pricing.get("input", 3.00)
        output_cost = (output_tokens / 1_000_000) * pricing.get("output", 15.00)
        
        return round(input_cost + output_cost, 4)
    
    def summary(self, days: int = 30) -> dict:
        """Get cost summary for the last N days."""
        cutoff = time.time() - (days * 86400)
        recent = [r for r in self.records if r.timestamp > cutoff]
        
        total_cost = sum(r.cost_if_api for r in recent)
        total_input = sum(r.input_tokens for r in recent)
        total_output = sum(r.output_tokens for r in recent)
        
        by_model = {}
        for r in recent:
            if r.model not in by_model:
                by_model[r.model] = {"calls": 0, "cost": 0, "tokens": 0}
            by_model[r.model]["calls"] += 1
            by_model[r.model]["cost"] += r.cost_if_api
            by_model[r.model]["tokens"] += r.input_tokens + r.output_tokens
        
        return {
            "period_days": days,
            "total_calls": len(recent),
            "total_tokens": total_input + total_output,
            "total_cost_if_api": f"${total_cost:.2f}",
            "actual_cost": "$0.00",
            "savings": f"${total_cost:.2f}",
            "by_model": {
                model: {
                    "calls": s["calls"],
                    "cost_if_api": f"${s['cost']:.2f}",
                    "tokens": s["tokens"],
                }
                for model, s in sorted(by_model.items(), key=lambda x: -x[1]["cost"])
            },
        }
