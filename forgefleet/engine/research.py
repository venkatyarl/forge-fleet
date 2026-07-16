"""Research — competitor monitoring, trend analysis, business research.

Autonomous research when fleet is idle: what's Workday doing?
What's trending in HR tech? New patent opportunities?
"""
import time
from dataclasses import dataclass, field
from .web_research import WebResearcher, SearchResult
from .llm import LLM


@dataclass
class ResearchReport:
    topic: str
    findings: list = field(default_factory=list)
    summary: str = ""
    sources: list = field(default_factory=list)
    timestamp: float = 0


class ResearchEngine:
    """Autonomous research using web search + LLM analysis."""
    
    COMPETITOR_QUERIES = {
        "HireFlow360": [
            "Workday HCM latest features 2026",
            "BambooHR new updates",
            "Deel EOR platform news",
            "Rippling HR platform features",
            "Gusto payroll updates",
        ],
        "FierceFlow": [
            "Stripe payments new features 2026",
            "Plaid fintech updates",
            "Mercury banking startup features",
        ],
        "MasterStaff": [
            "staffing agency software trends 2026",
            "EOR platform comparison",
        ],
    }
    
    TREND_QUERIES = [
        "AI in HR recruiting trends 2026",
        "employment law changes 2026",
        "remote work hiring trends",
        "HR technology market size growth",
    ]
    
    def __init__(self, llm: LLM = None):
        self.web = WebResearcher()
        self.llm = llm or LLM(base_url="http://192.168.5.100:51803/v1")
    
    def research_competitors(self, project: str = "HireFlow360") -> ResearchReport:
        """Research competitors for a specific project."""
        queries = self.COMPETITOR_QUERIES.get(project, self.COMPETITOR_QUERIES["HireFlow360"])
        
        all_results = []
        for query in queries:
            results = self.web.search(query, 3)
            all_results.extend(results)
        
        # Summarize with LLM
        findings_text = "\n".join(f"- {r.title}: {r.snippet}" for r in all_results)
        
        summary = self._summarize(
            f"Summarize these competitor findings for {project}. "
            f"Focus on: new features we should copy, threats to watch, opportunities.\n\n{findings_text}"
        )
        
        return ResearchReport(
            topic=f"Competitor analysis: {project}",
            findings=[{"title": r.title, "snippet": r.snippet, "url": r.url} for r in all_results],
            summary=summary,
            sources=[r.url for r in all_results],
            timestamp=time.time(),
        )
    
    def research_trends(self) -> ResearchReport:
        """Research industry trends."""
        all_results = []
        for query in self.TREND_QUERIES:
            results = self.web.search(query, 3)
            all_results.extend(results)
        
        findings_text = "\n".join(f"- {r.title}: {r.snippet}" for r in all_results)
        summary = self._summarize(
            f"Summarize HR/fintech industry trends from these findings. "
            f"Focus on actionable insights for a startup.\n\n{findings_text}"
        )
        
        return ResearchReport(
            topic="Industry trends",
            findings=[{"title": r.title, "snippet": r.snippet} for r in all_results],
            summary=summary,
            timestamp=time.time(),
        )
    
    def research_patents(self, topic: str) -> ResearchReport:
        """Research patent landscape for a topic."""
        results = self.web.search(f"patent {topic} prior art 2024 2025 2026", 5)
        
        findings_text = "\n".join(f"- {r.title}: {r.snippet}" for r in results)
        summary = self._summarize(
            f"Analyze the patent landscape for '{topic}'. "
            f"Are there existing patents? What's novel?\n\n{findings_text}"
        )
        
        return ResearchReport(
            topic=f"Patent research: {topic}",
            findings=[{"title": r.title, "snippet": r.snippet, "url": r.url} for r in results],
            summary=summary,
            timestamp=time.time(),
        )
    
    def _summarize(self, prompt: str) -> str:
        try:
            messages = [
                {"role": "system", "content": "You are a business research analyst. Be concise and actionable."},
                {"role": "user", "content": prompt},
            ]
            response = self.llm.call(messages)
            return response.get("content", "")
        except Exception:
            return "Summary generation failed"
