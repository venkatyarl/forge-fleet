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
        
        # Step 2: JUNIOR implements — ONE FILE AT A TIME
        print(f"\n👨‍💻 Junior implementing (file-by-file)...", flush=True)
        
        # Use intern's output to determine specific files to create
        # Break into individual file tasks for faster LLM calls
        file_tasks = self._extract_file_tasks(task_description, intern_result.output, tech_context)
        
        junior_result = HandoffResult(seniority=Seniority.JUNIOR, output="", status="complete", owner="Junior Developer")
        junior_start = time.time()
        
        for i, file_task in enumerate(file_tasks):
            print(f"  📄 File {i+1}/{len(file_tasks)}: {file_task[:60]}", flush=True)
            
            single_result = self._run_level(
                Seniority.JUNIOR,
                file_task,
                "",  # Minimal context — keep prompt small
                tech_context,
            )
            junior_result.output += f"\n{single_result.output}"
            
            if single_result.status == "failed":
                print(f"    ⚠️ Failed, continuing...", flush=True)
        
        junior_result.time_spent = time.time() - junior_start
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
        """Run one seniority level.
        
        Uses preferred tier but falls back to any available model.
        A Senior CAN do Junior work if Juniors are busy.
        Preference order: preferred tier → next tier up → next tier down.
        """
        config = SENIORITY_CONFIG[level]
        preferred_tier = config["tier"]
        
        # Try preferred tier first, then any available (all levels access all models)
        llm = self.router.get_llm(preferred_tier)
        if not llm:
            # Try ALL tiers — any model is better than no model
            for fallback_tier in sorted(range(1, 5), key=lambda t: abs(t - preferred_tier)):
                if fallback_tier != preferred_tier:
                    llm = self.router.get_llm(fallback_tier)
                    if llm:
                        print(f"  ℹ️ {config['title']}: using tier {fallback_tier} (preferred {preferred_tier} busy)", flush=True)
                        break
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
        try:
            output = agent.execute(task, context)
        except Exception as e:
            output = f"ERROR: {e}"
            print(f"  ❌ {config['title']} error: {str(e)[:100]}", flush=True)
        
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
    
    def _extract_file_tasks(self, task_description: str, intern_output: str, tech_context: str) -> list[str]:
        """Break a task into individual file-level instructions.
        
        Instead of "implement the whole feature," create:
        - "Create models.rs with Leave struct"
        - "Create handlers.rs with GET /leave endpoint"
        - "Create routes.rs with router setup"
        
        Each becomes a focused 32B call that finishes in <60s.
        """
        # Use 9B to decompose (fast)
        llm_fast = self.router.get_llm(1)
        if not llm_fast:
            llm_fast = LLM(base_url="http://192.168.5.100:51803/v1")
        
        prompt = f"""Break this task into individual FILE operations. 
Each item should be ONE write_file call.

Task: {task_description[:500]}
{f"Tech: {tech_context}" if tech_context else ""}
Research: {intern_output[:1000]}

List each file to create. Format — one per line:
FILE: path/to/file.rs — description of what goes in it

Only list files that need to be CREATED or MODIFIED. Max 5 files."""
        
        try:
            messages = [
                {"role": "system", "content": "List files to create. One per line. Format: FILE: path — description"},
                {"role": "user", "content": prompt},
            ]
            response = llm_fast.call(messages)
            content = response.get("content", "")
            
            # Parse FILE: lines
            import re
            file_lines = re.findall(r'FILE:\s*(.+)', content)
            
            if file_lines:
                tasks = []
                for fl in file_lines[:5]:
                    parts = fl.split("—", 1) if "—" in fl else fl.split("-", 1)
                    filepath = parts[0].strip()
                    desc = parts[1].strip() if len(parts) > 1 else "implement this file"
                    
                    tasks.append(
                        f"Create file: {filepath}\n"
                        f"Content: {desc}\n"
                        f"{tech_context}\n"
                        f"Use write_file to save it. Write the COMPLETE file content."
                    )
                return tasks
        except Exception:
            pass
        
        # Fallback: single task
        return [
            f"Implement this feature using write_file:\n{task_description[:500]}\n{tech_context}"
        ]
    
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
