"""Continuous Improvement — ForgeFleet uses MC + local LLMs to get better autonomously.

After every task batch, ForgeFleet:
1. Analyzes what worked/failed (evolution engine)
2. Uses a local LLM to brainstorm fixes
3. Creates MC tickets for its OWN improvements
4. Builds those improvements using its own crew
5. Tests and deploys the fixes
6. Repeat

ForgeFleet literally builds itself better.
"""
import json
import os
import time
from dataclasses import dataclass, field
from .evolution import EvolutionEngine, TaskRecord
from .mc_client import MCClient
from .llm import LLM
from .fleet_router import FleetRouter
from .research import ResearchEngine
from .web_research import WebResearcher


@dataclass
class ImprovementCycle:
    """One improvement cycle — analyze → plan → build → verify."""
    cycle_number: int
    started_at: float
    analysis: dict = field(default_factory=dict)
    tickets_created: list = field(default_factory=list)
    tickets_completed: list = field(default_factory=list)
    research_findings: list = field(default_factory=list)
    duration: float = 0


class ContinuousImprover:
    """ForgeFleet improves itself using MC tickets + local LLMs.
    
    Cycles:
    1. ANALYZE — what failed, what was slow, what patterns repeat
    2. RESEARCH — what are other tools doing better
    3. PLAN — create improvement tickets in MC
    4. BUILD — use ForgeFleet's own crew to implement fixes
    5. VERIFY — test improvements, record results
    6. LEARN — feed results back into evolution engine
    """
    
    def __init__(self):
        self.evolution = EvolutionEngine()
        self.mc = MCClient()
        self.router = FleetRouter()
        self.llm = self.router.get_llm(3) or LLM(base_url="http://192.168.5.100:51801/v1")  # 72B for analysis
        self.llm_fast = self.router.get_llm(1) or LLM(base_url="http://192.168.5.100:51803/v1")
        self.researcher = ResearchEngine(llm=self.llm_fast)
        self.web = WebResearcher()
        self.cycles: list[ImprovementCycle] = []
    
    def run_cycle(self) -> ImprovementCycle:
        """Run one full improvement cycle."""
        cycle = ImprovementCycle(
            cycle_number=len(self.cycles) + 1,
            started_at=time.time(),
        )
        
        print(f"\n🔄 Improvement Cycle #{cycle.cycle_number}", flush=True)
        
        # Step 1: ANALYZE
        print("  📊 Analyzing recent performance...", flush=True)
        cycle.analysis = self._analyze()
        
        # Step 2: RESEARCH
        print("  🔍 Researching improvements...", flush=True)
        cycle.research_findings = self._research()
        
        # Step 3: PLAN
        print("  📋 Creating improvement tickets...", flush=True)
        cycle.tickets_created = self._create_tickets(cycle.analysis, cycle.research_findings)
        
        cycle.duration = time.time() - cycle.started_at
        self.cycles.append(cycle)
        
        print(f"  ✅ Cycle complete in {cycle.duration:.0f}s", flush=True)
        print(f"     Tickets created: {len(cycle.tickets_created)}", flush=True)
        
        return cycle
    
    def _analyze(self) -> dict:
        """Analyze recent task performance using evolution engine + LLM."""
        # Get evolution data
        proposal = self.evolution.generate_version_proposal()
        
        # Use LLM to generate deeper analysis
        analysis_prompt = f"""Analyze ForgeFleet's recent performance and suggest improvements.

Performance data:
{json.dumps(proposal, indent=2, default=str)[:3000]}

You are ForgeFleet's product manager. Based on this data:
1. What are the top 3 problems to fix?
2. What new features would improve success rate?
3. What prompt engineering changes would help?
4. What workflow changes would reduce wasted iterations?
5. Are there any patterns in failures we should address?

Be specific and actionable. Output as JSON:
{{"problems": [...], "features": [...], "prompt_changes": [...], "workflow_changes": [...], "patterns": [...]}}"""
        
        try:
            messages = [
                {"role": "system", "content": "You are a senior engineering manager analyzing an AI coding agent's performance."},
                {"role": "user", "content": analysis_prompt},
            ]
            response = self.llm.call(messages)
            content = response.get("content", "{}")
            
            # Parse JSON from response
            import re
            json_match = re.search(r'\{.*\}', content, re.DOTALL)
            if json_match:
                return json.loads(json_match.group())
        except Exception:
            pass
        
        return proposal
    
    def _research(self) -> list[dict]:
        """Research what other AI coding tools are doing better."""
        findings = []
        
        research_queries = [
            "AI coding agent best practices 2026",
            "LLM tool calling improvement techniques",
            "code generation quality local LLM",
            "agentic coding workflow optimization",
        ]
        
        for query in research_queries[:2]:  # Limit to 2 to save time
            results = self.web.search(query, 3)
            for r in results:
                findings.append({
                    "query": query,
                    "title": r.title,
                    "snippet": r.snippet,
                    "url": r.url,
                })
        
        return findings
    
    def _create_tickets(self, analysis: dict, research: list) -> list[str]:
        """Create MC tickets for improvements."""
        tickets_created = []
        
        # From analysis problems
        problems = analysis.get("problems", analysis.get("proposals", []))
        for i, problem in enumerate(problems[:5]):
            if isinstance(problem, dict):
                title = problem.get("problem", problem.get("title", f"Improvement {i+1}"))[:80]
                desc = problem.get("fix", problem.get("description", ""))
            else:
                title = str(problem)[:80]
                desc = ""
            
            result = self.mc._request("POST", "/api/work-items", {
                "title": f"[ForgeFleet Self-Improve] {title}",
                "description": f"Auto-generated improvement ticket.\n\n{desc}\n\nSource: Evolution Engine Cycle",
                "status": "todo",
                "priority": "medium",
            })
            
            if "error" not in result:
                tickets_created.append(title)
        
        # From research insights
        for finding in research[:2]:
            title = f"[Research] {finding['title'][:60]}"
            self.mc._request("POST", "/api/work-items", {
                "title": title,
                "description": f"Research finding:\n{finding['snippet']}\n\nSource: {finding['url']}",
                "status": "blocked",  # Needs human review before acting
                "priority": "low",
            })
            tickets_created.append(title)
        
        return tickets_created
    
    def suggest_prompt_improvements(self) -> list[dict]:
        """Use LLM to suggest prompt template improvements based on failures."""
        # Get recent failed tasks
        rows = self.evolution.db.execute(
            "SELECT title, task_type, error, wasted_iterations FROM task_records WHERE success=0 ORDER BY timestamp DESC LIMIT 10"
        ).fetchall()
        
        if not rows:
            return []
        
        failures = "\n".join(f"- {r[0]} ({r[1]}): {r[2]}" for r in rows)
        
        try:
            messages = [
                {"role": "system", "content": "You are a prompt engineering expert for AI coding agents."},
                {"role": "user", "content": f"""These tasks FAILED in our AI coding agent:

{failures}

For each failure pattern, suggest a specific prompt template change that would fix it.
Output JSON array: [{{"pattern": "...", "current_issue": "...", "suggested_prompt_change": "...", "example": "..."}}]"""},
            ]
            response = self.llm.call(messages)
            content = response.get("content", "[]")
            
            import re
            json_match = re.search(r'\[.*\]', content, re.DOTALL)
            if json_match:
                return json.loads(json_match.group())
        except Exception:
            pass
        
        return []
    
    def suggest_new_features(self) -> list[dict]:
        """Use LLM to brainstorm features ForgeFleet should have."""
        current_modules = [
            "agent loop", "task decomposer", "fleet router", "auto-fix loop",
            "sandbox", "repo map", "code review", "gap analyzer", "evolution engine",
            "web research", "content generator", "trading monitor", "freelance scanner",
        ]
        
        try:
            messages = [
                {"role": "system", "content": "You are a product visionary for AI development tools."},
                {"role": "user", "content": f"""ForgeFleet is a distributed AI coding agent with these modules:
{', '.join(current_modules)}

It uses local LLMs (9B, 32B, 72B) across 6 computers to build code autonomously.

What features is it MISSING that would make it dramatically better?
Think about: developer experience, code quality, speed, intelligence, integration.

Output JSON array: [{{"feature": "...", "impact": "high/medium/low", "effort": "small/medium/large", "description": "..."}}]
Top 10 features only."""},
            ]
            response = self.llm.call(messages)
            content = response.get("content", "[]")
            
            import re
            json_match = re.search(r'\[.*\]', content, re.DOTALL)
            if json_match:
                return json.loads(json_match.group())
        except Exception:
            pass
        
        return []
    
    def status(self) -> dict:
        """Get continuous improvement status."""
        return {
            "cycles_completed": len(self.cycles),
            "total_tickets_created": sum(len(c.tickets_created) for c in self.cycles),
            "evolution_data": {
                "tasks_recorded": self.evolution.db.execute("SELECT COUNT(*) FROM task_records").fetchone()[0],
                "insights_generated": self.evolution.db.execute("SELECT COUNT(*) FROM insights").fetchone()[0],
                "proposals_pending": self.evolution.db.execute("SELECT COUNT(*) FROM version_proposals WHERE status='proposed'").fetchone()[0],
            },
            "last_cycle": {
                "number": self.cycles[-1].cycle_number if self.cycles else 0,
                "duration": self.cycles[-1].duration if self.cycles else 0,
                "tickets": len(self.cycles[-1].tickets_created) if self.cycles else 0,
            } if self.cycles else None,
        }
