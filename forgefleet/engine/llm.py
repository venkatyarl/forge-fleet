"""LLM — thin wrapper around OpenAI-compatible API (llama.cpp, Ollama, etc.)."""
import json
import urllib.request
import urllib.error
from dataclasses import dataclass


@dataclass
class LLM:
    """Calls any OpenAI-compatible chat/completions endpoint.
    
    Works with llama.cpp server, Ollama, vLLM, or any OpenAI-compatible API.
    No dependencies — uses urllib directly.
    """
    base_url: str = "http://localhost:8082/v1"
    model: str = "local"
    temperature: float = 0.2
    max_tokens: int = 4096
    timeout: int = 900  # 15 min for large models
    api_key: str = "not-needed"
    
    def call(self, messages: list[dict], tools: list[dict] = None) -> dict:
        """Send chat completion request. Returns the assistant message dict."""
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
        
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                result = json.loads(resp.read())
                return result["choices"][0]["message"]
        except urllib.error.HTTPError as e:
            body = e.read().decode() if e.fp else ""
            raise RuntimeError(f"LLM HTTP {e.code}: {body[:500]}") from e
        except urllib.error.URLError as e:
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
