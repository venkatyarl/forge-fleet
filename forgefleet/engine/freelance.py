"""Freelance — gig matching + proposal generation.

Scans Upwork/Freelancer for gigs matching our skills,
generates tailored proposals using local LLMs.
"""
import time
from dataclasses import dataclass, field
from .web_research import WebResearcher
from .llm import LLM


@dataclass
class Gig:
    title: str
    platform: str
    url: str
    budget: str
    skills_match: float  # 0-1
    description: str = ""


@dataclass
class Proposal:
    gig: Gig
    content: str
    estimated_hours: int = 0


class FreelanceEngine:
    """Find freelance gigs and generate proposals."""
    
    OUR_SKILLS = [
        "Rust", "TypeScript", "React", "Next.js", "PostgreSQL",
        "AI/ML", "LLM integration", "Docker", "AWS", "Python",
        "Full-stack development", "API design", "HR tech",
        "Fintech", "SaaS development", "System architecture",
    ]
    
    def __init__(self, llm: LLM = None):
        self.web = WebResearcher()
        self.llm = llm or LLM(base_url="http://192.168.5.100:51802/v1")
    
    def find_gigs(self, platform: str = "upwork", max_results: int = 5) -> list[Gig]:
        """Search for matching freelance gigs."""
        queries = [
            f"{platform} Rust developer remote contract 2026",
            f"{platform} React TypeScript Next.js developer",
            f"{platform} AI LLM integration developer",
            f"{platform} full stack SaaS developer contract",
        ]
        
        gigs = []
        for query in queries:
            results = self.web.search(query, 3)
            for r in results:
                if any(skill.lower() in r.title.lower() + r.snippet.lower() for skill in self.OUR_SKILLS[:5]):
                    gigs.append(Gig(
                        title=r.title, platform=platform,
                        url=r.url, budget="",
                        skills_match=self._score_match(r.title + r.snippet),
                        description=r.snippet,
                    ))
        
        gigs.sort(key=lambda g: -g.skills_match)
        return gigs[:max_results]
    
    def generate_proposal(self, gig: Gig) -> Proposal:
        """Generate a tailored proposal for a gig."""
        content = self._generate(f"""Write a freelance proposal for this gig:

Title: {gig.title}
Description: {gig.description}

Our strengths: {', '.join(self.OUR_SKILLS[:8])}

Write a compelling, personalized proposal that:
1. Opens with understanding their specific need
2. Shows relevant experience (Rust/React/AI projects)
3. Proposes a clear approach with timeline
4. Includes a competitive rate
5. Ends with a confident CTA

Keep under 300 words. Professional but not generic.""")
        
        return Proposal(gig=gig, content=content)
    
    def _score_match(self, text: str) -> float:
        text_lower = text.lower()
        matches = sum(1 for skill in self.OUR_SKILLS if skill.lower() in text_lower)
        return min(1.0, matches / 5)
    
    def _generate(self, prompt: str) -> str:
        try:
            messages = [
                {"role": "system", "content": "You are an expert freelance proposal writer."},
                {"role": "user", "content": prompt},
            ]
            response = self.llm.call(messages)
            return response.get("content", "")
        except Exception as e:
            return f"Generation failed: {e}"
    
    def daily_scan(self) -> str:
        """Daily scan for new gigs."""
        gigs = self.find_gigs("upwork", 5)
        
        if not gigs:
            return "No matching gigs found today"
        
        lines = ["💼 Daily Gig Scan\n"]
        for g in gigs:
            match_pct = int(g.skills_match * 100)
            lines.append(f"  {match_pct}% match: {g.title[:60]}")
            lines.append(f"    {g.description[:80]}")
        
        return "\n".join(lines)
