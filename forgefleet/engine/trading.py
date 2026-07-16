"""Trading — Polymarket/crypto signal monitoring.

Monitors prediction markets and crypto for opportunities.
Uses web scraping (no API keys needed for basic monitoring).
"""
import time
from dataclasses import dataclass, field
from .web_research import WebResearcher
from .llm import LLM


@dataclass
class Signal:
    source: str  # "polymarket", "crypto", "news"
    market: str
    direction: str  # "bullish", "bearish", "neutral"
    confidence: float  # 0-1
    reasoning: str
    timestamp: float = 0


class TradingMonitor:
    """Monitor markets for trading signals."""
    
    def __init__(self, llm: LLM = None):
        self.web = WebResearcher()
        self.llm = llm or LLM(base_url="http://192.168.5.100:51803/v1")
    
    def scan_polymarket(self) -> list[Signal]:
        """Scan Polymarket for mispriced opportunities."""
        results = self.web.search("Polymarket trending markets high volume 2026", 5)
        
        findings = "\n".join(f"- {r.title}: {r.snippet}" for r in results)
        analysis = self._analyze(f"""Analyze these Polymarket trends. Find markets where the odds seem mispriced 
(where public sentiment doesn't match likely outcomes).

{findings}

For each opportunity, output: market name, current implied probability, your estimate, and reasoning.""")
        
        return [Signal(
            source="polymarket", market="general", direction="neutral",
            confidence=0.5, reasoning=analysis, timestamp=time.time(),
        )]
    
    def scan_crypto(self, tokens: list = None) -> list[Signal]:
        """Scan crypto markets for momentum signals."""
        if tokens is None:
            tokens = ["SOL", "BTC", "ETH"]
        
        signals = []
        for token in tokens:
            results = self.web.search(f"{token} price prediction trend analysis today", 3)
            findings = "\n".join(f"- {r.title}: {r.snippet}" for r in results)
            
            analysis = self._analyze(f"""Quick {token} market analysis based on:
{findings}

Output: bullish/bearish/neutral, confidence (0-1), one-sentence reasoning.""")
            
            direction = "neutral"
            if "bullish" in analysis.lower():
                direction = "bullish"
            elif "bearish" in analysis.lower():
                direction = "bearish"
            
            signals.append(Signal(
                source="crypto", market=token, direction=direction,
                confidence=0.5, reasoning=analysis[:300], timestamp=time.time(),
            ))
        
        return signals
    
    def daily_brief(self) -> str:
        """Generate a daily market brief."""
        crypto = self.scan_crypto(["SOL", "BTC"])
        
        lines = ["📊 Daily Market Brief\n"]
        for sig in crypto:
            icon = {"bullish": "🟢", "bearish": "🔴", "neutral": "⚪"}.get(sig.direction, "⚪")
            lines.append(f"{icon} {sig.market}: {sig.direction} — {sig.reasoning[:100]}")
        
        return "\n".join(lines)
    
    def _analyze(self, prompt: str) -> str:
        try:
            messages = [
                {"role": "system", "content": "You are a market analyst. Be concise and data-driven. This is NOT financial advice."},
                {"role": "user", "content": prompt},
            ]
            response = self.llm.call(messages)
            return response.get("content", "")
        except Exception:
            return "Analysis unavailable"
