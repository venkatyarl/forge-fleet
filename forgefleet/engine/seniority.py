"""Seniority System — Intern/Junior/Senior/Architect hierarchy with handoffs.

Maps LLM tiers to seniority levels. Each level has clear responsibilities.
The model that finishes the work OWNS the commit.
Handoffs happen when a level can't complete the work.
"""
import time
from dataclasses import dataclass, field
from enum import Enum
from .agent import Agent
from .llm import LLM
from .tool import Tool
from .fleet_router import FleetRouter


class Seniority(Enum):
    INTERN = 1      # 9B — research, scaffold
    JUNIOR = 2      # 32B — implement
    SENIOR = 3      # 72B — review, fix, approve
    ARCHITECT = 4   # 235B — design, hard problems


SENIORITY_CONFIG = {
    Seniority.INTERN: {
        "tier": 1,
        "title": "Intern",
        "can_do": ["research", "list_files", "read_files", "scaffold"],
        "cannot_do": ["write_complex_code", "review", "approve", "architect"],
        "max_iterations": 8,
        "prompt": "You are an Intern. Your job is to RESEARCH and PREPARE, not write code. "
                  "Use list_files and read_file to understand the codebase. "
                  "Output a specific plan: which files to create, what each should contain, "
                  "and the exact structure. Be very detailed so a junior developer can implement it.",
    },
    Seniority.JUNIOR: {
        "tier": 2,
        "title": "Junior Developer",
        "can_do": ["write_code", "create_files", "run_tests", "simple_fixes"],
        "cannot_do": ["review", "approve", "architecture_decisions"],
        "max_iterations": 12,
        "prompt": "You are a Junior Developer. Your job is to WRITE CODE using the write_file tool. "
                  "You will receive specific instructions from the research phase. "
                  "Follow them exactly. Use write_file for EVERY file. "
                  "Do NOT describe what to do — WRITE THE CODE and save it.",
    },
    Seniority.SENIOR: {
        "tier": 3,
        "title": "Senior Developer",
        "can_do": ["review", "fix_bugs", "complex_code", "approve", "commit"],
        "cannot_do": ["architecture_overhaul"],
        "max_iterations": 8,
        "prompt": "You are a Senior Developer. Review the code, fix issues, and approve. "
                  "If the code has bugs, fix them using write_file. "
                  "If it's fundamentally wrong, say ESCALATE and explain why. "
                  "If it's good, say APPROVED.",
    },
    Seniority.ARCHITECT: {
        "tier": 4,
        "title": "Software Architect",
        "can_do": ["everything", "architecture", "design", "final_decisions"],
        "cannot_do": [],
        "max_iterations": 6,
        "prompt": "You are a Software Architect. You handle the hardest problems. "
                  "Design the solution, write the code if needed, make final decisions. "
                  "Your word is final on technical matters.",
    },
}


@dataclass
class HandoffResult:
    """Result of work at one seniority level."""
    seniority: Seniority
    output: str
    files_changed: list = field(default_factory=list)
    status: str = "complete"  # "complete", "escalate", "failed"
    escalation_reason: str = ""
    time_spent: float = 0
    owner: str = ""  # Who owns the commit


class SeniorityPipeline:
    """Execute a ticket through the seniority hierarchy.
    
    Flow:
    1. Intern researches → produces detailed instructions
    2. Junior implements → produces code files
    3. Senior reviews → approves or fixes
    4. If Senior says ESCALATE → Architect takes over
    
    The level that produces the FINAL approved code owns the commit.
    """
    
    def __init__(self, tools: list[Tool], router: FleetRouter = None):
        self.tools = tools
        self.router = router or FleetRouter()
    
    def execute(self, task_description: str, tech_stack: dict = None) -> dict:
        """Run through the full seniority pipeline."""
        start = time.time()
        results = []
        
        tech_context = ""
        if tech_stack:
            if tech_stack.get("backend") == "Rust + Axum":
                tech_context = "This is a RUST + Axum + PostgreSQL project. Write RUST code only."
            if tech_stack.get("frontend"):
                tech_context += f" Frontend: {tech_stack['frontend']}."
        
        # Step 1: INTERN researches
        print(f"\n👶 Intern researching...", flush=True)
        intern_result = self._run_level(
            Seniority.INTERN, task_description, "", tech_context
        )
        results.append(intern_result)
        
        if intern_result.status == "failed":
            return self._build_result(results, time.time() - start)
        
        # Step 2: JUNIOR implements based on intern's research
        print(f"\n👨‍💻 Junior implementing...", flush=True)
        junior_instructions = (
            f"The research team found this about the codebase:\n{intern_result.output[:3000]}\n\n"
            f"Now implement the feature. Use write_file for EVERY file you create.\n"
            f"Task: {task_description}"
        )
        junior_result = self._run_level(
            Seniority.JUNIOR, junior_instructions, intern_result.output, tech_context
        )
        results.append(junior_result)
        
        if junior_result.status == "failed" and not junior_result.files_changed:
            # Junior failed — escalate directly to Senior to implement
            print(f"  ⬆️ Junior failed, escalating to Senior", flush=True)
            senior_impl = self._run_level(
                Seniority.SENIOR,
                f"The junior developer couldn't implement this. Please implement it yourself:\n{task_description}",
                intern_result.output, tech_context
            )
            results.append(senior_impl)
            return self._build_result(results, time.time() - start)
        
        # Step 3: SENIOR reviews
        print(f"\n👨‍🔬 Senior reviewing...", flush=True)
        review_context = (
            f"Review this implementation:\n{junior_result.output[:2000]}\n\n"
            f"Original task: {task_description}\n\n"
            f"Check: correct tech stack, no placeholders, error handling, tests?"
        )
        senior_result = self._run_level(
            Seniority.SENIOR, review_context, junior_result.output, tech_context
        )
        results.append(senior_result)
        
        # Check if Senior approved or wants to escalate
        if "ESCALATE" in senior_result.output.upper():
            print(f"\n🏛️ Architect taking over...", flush=True)
            architect_result = self._run_level(
                Seniority.ARCHITECT,
                f"Senior developer escalated this:\n{senior_result.escalation_reason or senior_result.output[:1000]}\n\nOriginal task: {task_description}",
                intern_result.output, tech_context
            )
            results.append(architect_result)
        
        return self._build_result(results, time.time() - start)
    
    def _run_level(self, level: Seniority, task: str, context: str, tech_context: str) -> HandoffResult:
        """Run one seniority level."""
        config = SENIORITY_CONFIG[level]
        
        llm = self.router.get_llm(config["tier"])
        if not llm:
            llm = LLM(base_url="http://192.168.5.100:51803/v1")
        
        # Select tools based on seniority
        if level == Seniority.INTERN:
            level_tools = [t for t in self.tools if t.name in ("read_file", "list_files", "run_command")]
        elif level == Seniority.JUNIOR:
            level_tools = self.tools  # All tools including write_file
        else:
            level_tools = self.tools  # Seniors and Architects get everything
        
        backstory = config["prompt"]
        if tech_context:
            backstory += f"\n\nIMPORTANT: {tech_context}"
        
        agent = Agent(
            role=config["title"],
            goal=f"Complete your part of the work as a {config['title']}",
            backstory=backstory,
            tools=level_tools,
            llm=llm,
            verbose=True,
            max_iterations=config["max_iterations"],
        )
        
        start = time.time()
        output = agent.execute(task, context)
        
        result = HandoffResult(
            seniority=level,
            output=output,
            status="complete",
            time_spent=time.time() - start,
            owner=config["title"],
        )
        
        # Check for escalation
        if "ESCALATE" in output.upper():
            result.status = "escalate"
            result.escalation_reason = output
        
        return result
    
    def _build_result(self, results: list[HandoffResult], total_time: float) -> dict:
        """Build the final result, determining who owns the commit."""
        # The last level that produced output owns the commit
        owner = "unknown"
        for r in reversed(results):
            if r.output and r.status != "failed":
                owner = r.owner
                break
        
        return {
            "success": any(r.status == "complete" for r in results),
            "owner": owner,
            "total_time": round(total_time, 1),
            "levels_used": [r.seniority.name for r in results],
            "escalations": sum(1 for r in results if r.status == "escalate"),
            "results": [
                {
                    "level": r.seniority.name,
                    "status": r.status,
                    "time": round(r.time_spent, 1),
                    "output_length": len(r.output),
                }
                for r in results
            ],
        }
