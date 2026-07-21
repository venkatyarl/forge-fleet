import requests
from typing import List, Dict


class LocalLLMClient:
    """Wrapper for local LLM APIs (e.g., Ollama, llama.cpp)."""
    
    def __init__(self, endpoint: str, model: str):
        self.endpoint = endpoint.rstrip("/")
        self.model = model

    def chat(self, messages: List[Dict[str, str]]) -> str:
        """Send a chat request to the local LLM endpoint and return the assistant's response."""
        url = f"{self.endpoint}/api/chat"
        payload = {
            "model": self.model,
            "messages": messages,
            "stream": False,
        }
        response = requests.post(url, json=payload, timeout=300)
        response.raise_for_status()
        data = response.json()
        return data.get("message", {}).get("content", "")
