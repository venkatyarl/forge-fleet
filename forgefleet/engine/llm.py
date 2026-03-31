"""LLM — thin wrapper around OpenAI-compatible API (llama.cpp, Ollama, etc.)."""
import json
import time
import urllib.request
import urllib.error
from dataclasses import dataclass

from .model_governance import ModelGovernance, TaskRunRecord


@dataclass
class LLM:
    """Calls any OpenAI-compatible chat/completions endpoint.
    
    Works with llama.cpp server, Ollama, vLLM, or any OpenAI-compatible API.
    No dependencies — uses urllib directly.
    """
    base_url: str = "http://localhost:51803/v1"
    model: str = "local"
    temperature: float = 0.2
    max_tokens: int = 4096
    timeout: int = 900  # 15 min for large models
    api_key: str = "not-needed"
    governance: ModelGovernance | None = None
    
    def call(self, messages: list[dict], tools: list[dict] = None,
             task_type: str = "general", node: str = "", mode: str = "single") -> dict:
        """Send chat completion request. Returns the assistant message dict.

        Phase 1 governance hook:
        - records each task/model run into ForgeFleet's governance DB
        - keeps rough token estimates for now until richer provider stats are added
        """
        payload = {
            "model": self.model,
            "messages": messages,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
        }
        if tools:
            payload["tools"] = tools
            payload["tool_choice"] = "auto"
        
        data = json.dumps(payload).encode()
        req = urllib.request.Request(
            f"{self.base_url}/chat/completions",
            data=data,
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {self.api_key}",
            },
        )
        
        start = time.time()
        prompt_summary = " | ".join(m.get("content", "")[:120] for m in messages if isinstance(m.get("content"), str))[:240]
        estimated_input_tokens = max(1, len(json.dumps(messages)) // 4)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                result = json.loads(resp.read())
                message = result["choices"][0]["message"]
                content = message.get("content", "") if isinstance(message, dict) else ""
                estimated_output_tokens = max(1, len(content) // 4) if content else 0
                latency_ms = int((time.time() - start) * 1000)
                if self.governance:
                    self.governance.record_task_run(TaskRunRecord(
                        task_type=task_type,
                        mode=mode,
                        model_id=self.model,
                        node=node,
                        prompt_summary=prompt_summary,
                        success=True,
                        latency_ms=latency_ms,
                        input_tokens=estimated_input_tokens,
                        output_tokens=estimated_output_tokens,
                        metadata={"base_url": self.base_url, "tools_used": bool(tools)},
                    ))
                return message
        except urllib.error.HTTPError as e:
            body = e.read().decode() if e.fp else ""
            latency_ms = int((time.time() - start) * 1000)
            if self.governance:
                self.governance.record_task_run(TaskRunRecord(
                    task_type=task_type,
                    mode=mode,
                    model_id=self.model,
                    node=node,
                    prompt_summary=prompt_summary,
                    success=False,
                    latency_ms=latency_ms,
                    input_tokens=estimated_input_tokens,
                    output_tokens=0,
                    metadata={"error": f"HTTP {e.code}", "body": body[:300], "base_url": self.base_url},
                ))
            raise RuntimeError(f"LLM HTTP {e.code}: {body[:500]}") from e
        except urllib.error.URLError as e:
            latency_ms = int((time.time() - start) * 1000)
            if self.governance:
                self.governance.record_task_run(TaskRunRecord(
                    task_type=task_type,
                    mode=mode,
                    model_id=self.model,
                    node=node,
                    prompt_summary=prompt_summary,
                    success=False,
                    latency_ms=latency_ms,
                    input_tokens=estimated_input_tokens,
                    output_tokens=0,
                    metadata={"error": f"URL error: {e.reason}", "base_url": self.base_url},
                ))
            raise RuntimeError(f"LLM connection failed: {e.reason}") from e
    
    def health(self) -> bool:
        """Check if the LLM endpoint is healthy."""
        try:
            url = self.base_url.rstrip("/v1").rstrip("/")
            req = urllib.request.Request(f"{url}/health")
            with urllib.request.urlopen(req, timeout=5) as resp:
                return resp.status == 200
        except Exception:
            return False
