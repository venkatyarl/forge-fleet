"""Model Benchmarker — test which model is best for which task type.

Item #8: Before routing a task, benchmark models to find the best fit.
Item #15: A/B testing — same task, two models, compare results.
"""
import json
import os
import time
from dataclasses import dataclass, field
from .llm import LLM


@dataclass
class BenchmarkResult:
    """Result of a single benchmark run."""
    model: str
    tier: int
    task_type: str
    prompt: str
    response: str = ""
    tokens_generated: int = 0
    time_to_first_token: float = 0
    total_time: float = 0
    tokens_per_second: float = 0
    quality_score: float = 0  # 0-1, assessed by a judge model
    success: bool = False
    error: str = ""


class ModelBenchmarker:
    """Benchmark models for speed, quality, and task fitness.
    
    Tests:
    1. Speed: tokens/second, time-to-first-token
    2. Quality: judge model rates the response (or regex checks)
    3. Task fitness: which model is best for code/research/review
    """
    
    BENCHMARK_PROMPTS = {
        "code_simple": "Write a Python function that reverses a string. Include type hints and a docstring.",
        "code_complex": "Write a Rust function that implements a thread-safe LRU cache with TTL expiration. Use generics.",
        "reasoning": "A farmer has a fox, a chicken, and a bag of grain. He needs to cross a river in a boat that can only carry him and one item at a time. How does he do it?",
        "summarize": "Summarize the key differences between REST APIs and GraphQL in 3 bullet points.",
        "review": "Review this code for bugs:\n```rust\nfn divide(a: i32, b: i32) -> i32 {\n    a / b\n}\n```",
    }
    
    def __init__(self, results_path: str = ""):
        if not results_path:
            results_dir = os.path.expanduser("~/.forgefleet")
            os.makedirs(results_dir, exist_ok=True)
            results_path = os.path.join(results_dir, "benchmarks.json")
        self.results_path = results_path
        self.results: list[BenchmarkResult] = []
    
    def benchmark_model(self, llm: LLM, task_type: str = "code_simple",
                        custom_prompt: str = "") -> BenchmarkResult:
        """Benchmark a single model on a task."""
        prompt = custom_prompt or self.BENCHMARK_PROMPTS.get(task_type, task_type)
        
        result = BenchmarkResult(
            model=llm.model, tier=0,
            task_type=task_type, prompt=prompt[:100],
        )
        
        start = time.time()
        try:
            messages = [
                {"role": "system", "content": "Respond concisely and accurately."},
                {"role": "user", "content": prompt},
            ]
            response = llm.call(messages)
            result.total_time = round(time.time() - start, 2)
            result.response = response.get("content", "")
            result.tokens_generated = len(result.response.split())  # Rough estimate
            result.tokens_per_second = round(
                result.tokens_generated / max(result.total_time, 0.01), 1
            )
            result.success = bool(result.response)
            
            # Simple quality scoring
            result.quality_score = self._score_quality(task_type, result.response)
            
        except Exception as e:
            result.total_time = round(time.time() - start, 2)
            result.error = str(e)
        
        self.results.append(result)
        return result
    
    def ab_test(self, llm_a: LLM, llm_b: LLM, task_type: str = "code_simple",
                custom_prompt: str = "") -> dict:
        """A/B test: same prompt, two models, compare results."""
        result_a = self.benchmark_model(llm_a, task_type, custom_prompt)
        result_b = self.benchmark_model(llm_b, task_type, custom_prompt)
        
        winner = "A" if result_a.quality_score > result_b.quality_score else "B"
        if result_a.quality_score == result_b.quality_score:
            winner = "A" if result_a.total_time < result_b.total_time else "B"
        
        return {
            "model_a": {"model": llm_a.model, "time": result_a.total_time,
                       "quality": result_a.quality_score, "tps": result_a.tokens_per_second},
            "model_b": {"model": llm_b.model, "time": result_b.total_time,
                       "quality": result_b.quality_score, "tps": result_b.tokens_per_second},
            "winner": winner,
            "task_type": task_type,
        }
    
    def _score_quality(self, task_type: str, response: str) -> float:
        """Simple quality scoring without needing a judge model."""
        if not response:
            return 0.0
        
        score = 0.5  # Base score for any response
        
        if task_type.startswith("code"):
            # Code quality checks
            if "```" in response or "fn " in response or "def " in response:
                score += 0.2  # Has code blocks
            if "todo" in response.lower() or "placeholder" in response.lower():
                score -= 0.3  # Penalize placeholders
            if len(response) > 100:
                score += 0.1  # Substantive response
            if "error" in response.lower()[:50]:
                score -= 0.2  # Starts with error
        
        elif task_type == "review":
            if "bug" in response.lower() or "issue" in response.lower() or "divide by zero" in response.lower():
                score += 0.3  # Found the bug
        
        elif task_type == "reasoning":
            if "chicken" in response.lower() and "fox" in response.lower():
                score += 0.2  # Understood the problem
        
        return max(0.0, min(1.0, score))
    
    def leaderboard(self, task_type: str = None) -> list[dict]:
        """Get model leaderboard, optionally filtered by task type."""
        filtered = self.results
        if task_type:
            filtered = [r for r in filtered if r.task_type == task_type]
        
        # Group by model
        by_model = {}
        for r in filtered:
            if r.model not in by_model:
                by_model[r.model] = {"runs": 0, "total_quality": 0, "total_time": 0, "total_tps": 0}
            by_model[r.model]["runs"] += 1
            by_model[r.model]["total_quality"] += r.quality_score
            by_model[r.model]["total_time"] += r.total_time
            by_model[r.model]["total_tps"] += r.tokens_per_second
        
        leaderboard = []
        for model, stats in by_model.items():
            n = stats["runs"]
            leaderboard.append({
                "model": model,
                "runs": n,
                "avg_quality": round(stats["total_quality"] / n, 2),
                "avg_time": round(stats["total_time"] / n, 2),
                "avg_tps": round(stats["total_tps"] / n, 1),
            })
        
        leaderboard.sort(key=lambda x: (-x["avg_quality"], x["avg_time"]))
        return leaderboard
    
    def save(self):
        """Save results to disk."""
        data = [
            {"model": r.model, "task": r.task_type, "quality": r.quality_score,
             "time": r.total_time, "tps": r.tokens_per_second, "success": r.success}
            for r in self.results
        ]
        with open(self.results_path, "w") as f:
            json.dump(data, f, indent=2)
