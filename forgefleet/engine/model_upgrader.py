"""Model Upgrader — auto-download, benchmark, and swap better models.

Item #11: When a new Qwen model drops, auto-download + benchmark + swap if better.
Upstream monitor detects the release, model_upgrader handles the rest.
"""
import json
import os
import subprocess
import time
from dataclasses import dataclass, field
from .benchmarker import ModelBenchmarker, BenchmarkResult
from .llm import LLM


@dataclass
class ModelCandidate:
    """A model candidate for upgrade."""
    name: str
    url: str
    size_gb: float
    tier: int
    source: str  # "huggingface", "ollama", "url"


class ModelUpgrader:
    """Automatically upgrade models when better ones are available.
    
    Flow:
    1. Upstream monitor detects new release
    2. ModelUpgrader downloads to staging area
    3. Starts on a temp port
    4. Benchmarks against current model
    5. If better → swap. If worse → delete.
    """
    
    def __init__(self, staging_dir: str = ""):
        if not staging_dir:
            staging_dir = os.path.expanduser("~/models/staging")
        self.staging_dir = staging_dir
        os.makedirs(staging_dir, exist_ok=True)
        self.benchmarker = ModelBenchmarker()
    
    def download(self, url: str, filename: str) -> str:
        """Download a model to the staging area."""
        filepath = os.path.join(self.staging_dir, filename)
        
        if os.path.exists(filepath):
            return filepath
        
        print(f"📥 Downloading {filename}...")
        try:
            r = subprocess.run(
                ["wget", "-q", "--show-progress", "-O", filepath, url],
                timeout=7200,  # 2 hours max for large models
            )
            if r.returncode == 0 and os.path.exists(filepath):
                size_gb = os.path.getsize(filepath) / (1024**3)
                print(f"  ✅ Downloaded: {size_gb:.1f}GB")
                return filepath
        except Exception as e:
            print(f"  ❌ Download failed: {e}")
        
        return ""
    
    def benchmark_candidate(self, model_path: str, port: int = 51899,
                           task_types: list = None) -> dict:
        """Benchmark a candidate model on a temp port."""
        if task_types is None:
            task_types = ["code_simple", "code_complex", "review"]
        
        # Start model on temp port
        proc = subprocess.Popen(
            ["llama-server", "--model", model_path, "--port", str(port),
             "--host", "127.0.0.1", "--ctx-size", "8192", "--n-gpu-layers", "99"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        
        # Wait for it to load
        time.sleep(10)
        
        llm = LLM(base_url=f"http://127.0.0.1:{port}/v1")
        
        results = {}
        for task_type in task_types:
            try:
                result = self.benchmarker.benchmark_model(llm, task_type)
                results[task_type] = {
                    "quality": result.quality_score,
                    "time": result.total_time,
                    "tps": result.tokens_per_second,
                }
            except Exception as e:
                results[task_type] = {"error": str(e)}
        
        # Kill temp server
        proc.terminate()
        proc.wait(timeout=10)
        
        return results
    
    def compare_and_swap(self, candidate_path: str, current_endpoint: str,
                         current_model_path: str, node: str = "localhost") -> dict:
        """Compare candidate against current model, swap if better."""
        # Benchmark candidate
        candidate_results = self.benchmark_candidate(candidate_path)
        
        # Benchmark current
        current_llm = LLM(base_url=f"{current_endpoint}/v1")
        current_results = {}
        for task_type in candidate_results:
            if "error" not in candidate_results[task_type]:
                result = self.benchmarker.benchmark_model(current_llm, task_type)
                current_results[task_type] = {
                    "quality": result.quality_score,
                    "time": result.total_time,
                    "tps": result.tokens_per_second,
                }
        
        # Compare
        candidate_avg = sum(
            r.get("quality", 0) for r in candidate_results.values() if "error" not in r
        )
        current_avg = sum(
            r.get("quality", 0) for r in current_results.values()
        )
        
        num_tasks = len([r for r in candidate_results.values() if "error" not in r])
        
        if num_tasks > 0:
            candidate_avg /= num_tasks
            current_avg /= max(len(current_results), 1)
        
        should_swap = candidate_avg > current_avg * 1.05  # 5% improvement threshold
        
        return {
            "candidate_score": round(candidate_avg, 3),
            "current_score": round(current_avg, 3),
            "improvement": f"{((candidate_avg/max(current_avg, 0.01))-1)*100:.1f}%",
            "should_swap": should_swap,
            "details": {
                "candidate": candidate_results,
                "current": current_results,
            },
        }
