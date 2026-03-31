"""Full Engineering Pipeline — the 6-step process for every ticket.

1. Context Gathering — understand the project, related tickets, tech stack
2. Planning — game plan, research, model selection
3. Multi-Perspective Pre-Review — roles analyze the plan BEFORE building
4. Build — decompose, code, test, fix
5. Multi-Perspective Post-Review — roles verify AFTER building
6. Completion — commit, push, unblock dependents
"""
import json
import os
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from .agent import Agent
from .task import Task
from .crew import Crew
from .llm import LLM
from .tool import Tool
from .fleet_router import FleetRouter
from .mc_client import MCClient
from .git_ops import GitOps
from .roles import Role, PRE_BUILD_ROLES, POST_BUILD_ROLES, ALL_ROLES
from .repo_map import RepoMap
from .prompt_templates import get_template
from .evolution import EvolutionEngine, TaskRecord
from .task_decomposer import TaskDecomposer
from .context_store import ContextStore
from .ownership import OwnershipManager
from .execution_tracking import ExecutionTracker


@dataclass
class PipelineResult:
    """Result of the full pipeline for one ticket."""
    ticket_id: str
    title: str
    success: bool = False
    phase_results: dict = field(default_factory=dict)
    files_changed: list = field(default_factory=list)
    branch: str = ""
    total_time: float = 0
    pre_review_issues: list = field(default_factory=list)
    post_review_issues: list = field(default_factory=list)
    prerequisite_tickets: list = field(default_factory=list)
    unblocked_tickets: list = field(default_factory=list)


class EngineeringPipeline:
    """The full 6-step engineering pipeline.
    
    Every ticket goes through all 6 steps.
    Each step uses the right LLM tier and runs perspectives in parallel.
    """
    
    def __init__(self, repo_dir: str, mc_url: str = "http://192.168.5.100:60002",
                 ownership: OwnershipManager | None = None):
        self.repo_dir = repo_dir
        self.router = FleetRouter()
        self.mc = MCClient(base_url=mc_url)
        self.git = GitOps(repo_dir)
        self.evolution = EvolutionEngine()
        self.context_store = ContextStore()
        self.repo_map = RepoMap(repo_dir)
        self.tools = self._build_tools()
        self.ownership = ownership
    
    def execute(self, ticket: dict) -> PipelineResult:
        """Execute the full pipeline for a ticket."""
        tid = ticket["id"]
        title = ticket.get("title", "")
        desc = ticket.get("description", title)
        
        result = PipelineResult(ticket_id=tid, title=title)
        start = time.time()
        
        print(f"\n{'='*60}", flush=True)
        print(f"🎯 Pipeline: {title[:60]}", flush=True)
        
        try:
            # Step 1: CONTEXT GATHERING
            print(f"\n📚 Step 1: Context Gathering", flush=True)
            context = self._gather_context(ticket)
            result.phase_results["context"] = context
            self._record_stage_model(tid, "context_gathering")
            
            # Step 2: PLANNING
            print(f"\n📋 Step 2: Planning", flush=True)
            plan = self._create_plan(ticket, context)
            result.phase_results["plan"] = plan
            self._record_stage_model(tid, "planning")
            
            # Step 3: PRE-BUILD MULTI-PERSPECTIVE REVIEW
            print(f"\n🔍 Step 3: Pre-Build Review ({len(PRE_BUILD_ROLES)} perspectives)", flush=True)
            pre_issues = self._multi_perspective_review(plan, PRE_BUILD_ROLES, "pre")
            result.pre_review_issues = pre_issues
            result.phase_results["pre_review"] = pre_issues
            self._record_stage_model(tid, "pre_review")

            # Add pre-build reviewers as contributors
            if self.ownership:
                for role in PRE_BUILD_ROLES:
                    self.ownership.add_contributor(tid, role.name if hasattr(role, 'name') else str(role))
            
            # Check for prerequisites
            prereqs = [i for i in pre_issues if "prerequisite" in i.lower() or "dependency" in i.lower() or "blocked" in i.lower()]
            if prereqs:
                result.prerequisite_tickets = self._create_prerequisite_tickets(tid, prereqs)
                print(f"  ⚠️ Created {len(result.prerequisite_tickets)} prerequisite tickets", flush=True)
            
            # Step 4: BUILD
            print(f"\n🔨 Step 4: Build", flush=True)
            branch = f"feat/forgefleet-{tid[:8]}"
            self.git.create_branch(branch)
            build_result = self._build(desc, context, plan)
            result.phase_results["build"] = build_result
            result.branch = branch
            self._record_stage_model(tid, "build")
            
            # Step 5: POST-BUILD MULTI-PERSPECTIVE REVIEW
            if self.git.has_changes():
                print(f"\n🔬 Step 5: Post-Build Review ({len(POST_BUILD_ROLES)} perspectives)", flush=True)
                
                # Get the diff for review
                diff = self.git._run("diff", "--cached").output if self.git.has_changes() else ""
                post_issues = self._multi_perspective_review(
                    f"Code changes:\n{diff[:3000]}\n\nOriginal task: {desc}",
                    POST_BUILD_ROLES, "post"
                )
                result.post_review_issues = post_issues
                result.phase_results["post_review"] = post_issues
                self._record_stage_model(tid, "post_review")

                # Add reviewers to ownership tracking
                if self.ownership:
                    for role in POST_BUILD_ROLES:
                        self.ownership.add_reviewer(tid, role.name if hasattr(role, 'name') else str(role))
                
                # Step 6: COMPLETION
                print(f"\n✅ Step 6: Completion", flush=True)
                self.git.stage_all()
                self.git.commit(f"feat: {title[:50]} [ForgeFleet Pipeline]")
                push = self.git.push(branch)
                
                if push.success:
                    result.success = True
                    result.branch = branch
                    self.mc.complete_ticket(tid, f"Built by ForgeFleet Pipeline in {time.time()-start:.0f}s", branch)
                    
                    # Unblock dependent tickets
                    result.unblocked_tickets = self._unblock_dependents(tid)
                    if result.unblocked_tickets:
                        print(f"  🔓 Unblocked {len(result.unblocked_tickets)} dependent tickets", flush=True)
                    
                    print(f"  ✅ Pushed to {branch}", flush=True)
                else:
                    self.mc.fail_ticket(tid, f"Push failed: {push.error}")
                    print(f"  ❌ Push failed", flush=True)
            else:
                self.mc.fail_ticket(tid, "No code changes produced")
                print(f"  ⚠️ No changes produced", flush=True)
            
        except Exception as e:
            result.phase_results["error"] = str(e)
            self.mc.fail_ticket(tid, str(e)[:500])
            print(f"  ❌ Pipeline error: {e}", flush=True)
            # Escalation trigger: if pipeline fails, escalate ownership
            if self.ownership:
                task = self.ownership.get_task(tid)
                if task and task.owner_level != "human":
                    ok, reason = self.ownership.escalate(tid)
                    if ok:
                        print(f"  ⬆️ Escalated to {reason}", flush=True)
        finally:
            self.git._run("checkout", "main")
        
        result.total_time = time.time() - start
        
        # Record in evolution engine
        self.evolution.record_task(TaskRecord(
            task_id=tid, title=title,
            task_type=self._detect_task_type(desc),
            total_time=result.total_time,
            success=result.success, pushed=result.success,
            error=result.phase_results.get("error", ""),
        ))
        
        return result
    
    def _record_stage_model(self, ticket_id: str, stage: str):
        """Record which model/node was used for a pipeline stage."""
        if not self.ownership:
            return
        # Use the last LLM the router handed out
        for ep in self.router.endpoints:
            if ep.busy:
                self.ownership.record_model(
                    ticket_id=ticket_id, stage=stage,
                    model_name=ep.name, node_name=ep.node, role="executor",
                )
                return
        # Fallback: record the first healthy endpoint
        for ep in self.router.endpoints:
            if ep.healthy:
                self.ownership.record_model(
                    ticket_id=ticket_id, stage=stage,
                    model_name=ep.name, node_name=ep.node, role="executor",
                )
                return

    # ─── Step 1: Context Gathering ──────────────────
    
    def _gather_context(self, ticket: dict) -> dict:
        """Understand the project, related tickets, tech stack."""
        tid = ticket["id"]
        
        # Get related tickets from MC
        all_tickets = self.mc.get_tickets()
        related = [t for t in all_tickets if t.get("parent_id") == tid or t.get("id") == ticket.get("parent_id")]
        
        # Detect tech stack from existing code
        tech_stack = self._detect_tech_stack()
        
        # Get repo map context
        if not self.repo_map.files:
            self.repo_map.build()
        
        relevant_files = self.repo_map.context_for_task(ticket.get("title", ""))
        
        return {
            "tech_stack": tech_stack,
            "related_tickets": len(related),
            "relevant_files": relevant_files[:2000],
            "repo_summary": self.repo_map.summary()[:1000],
        }
    
    def _detect_tech_stack(self) -> dict:
        """Detect project tech stack from existing files."""
        stack = {"backend": "", "frontend": "", "database": "", "language": ""}
        
        if os.path.exists(os.path.join(self.repo_dir, "Cargo.toml")):
            stack["backend"] = "Rust + Axum"
            stack["language"] = "Rust"
        if os.path.exists(os.path.join(self.repo_dir, "package.json")):
            try:
                pkg = json.loads(open(os.path.join(self.repo_dir, "package.json")).read())
                deps = pkg.get("dependencies", {})
                if "next" in deps:
                    stack["frontend"] = "Next.js + React + TypeScript"
                elif "react" in deps:
                    stack["frontend"] = "React + TypeScript"
            except Exception:
                stack["frontend"] = "Node.js"
        
        # Check for database
        for f in ["docker-compose.yml", "docker-compose.yaml"]:
            path = os.path.join(self.repo_dir, f)
            if os.path.exists(path):
                content = open(path).read()
                if "postgres" in content.lower():
                    stack["database"] = "PostgreSQL"
                elif "mysql" in content.lower():
                    stack["database"] = "MySQL"
        
        return stack
    
    # ─── Step 2: Planning ───────────────────────────
    
    def _create_plan(self, ticket: dict, context: dict) -> str:
        """Create a game plan using an LLM."""
        llm = self.router.get_llm(1) or LLM(base_url="http://192.168.5.100:51803/v1")
        
        messages = [
            {"role": "system", "content": "You are a tech lead creating a build plan. Be specific and actionable."},
            {"role": "user", "content": f"""Create a build plan for this ticket:

Title: {ticket.get('title', '')}
Description: {ticket.get('description', '')}

Project tech stack: {json.dumps(context.get('tech_stack', {}))}
Relevant files: {context.get('relevant_files', '')[:1000]}

Output:
1. What files need to be created/modified
2. What the implementation approach should be
3. Any risks or dependencies
4. Estimated complexity (simple/moderate/complex)"""},
        ]
        
        try:
            response = llm.call(messages)
            return response.get("content", "No plan generated")
        except Exception:
            return "Plan generation failed — proceeding with direct implementation"
    
    # ─── Step 3 & 5: Multi-Perspective Review ───────
    
    def _multi_perspective_review(self, content: str, roles: list[Role], phase: str) -> list[str]:
        """Run multiple role perspectives in PARALLEL across the fleet."""
        all_issues = []
        
        # Get available LLMs for parallel execution
        available_llms = []
        for tier in [3, 2, 1]:  # Prefer smarter models for review
            eps = self.router.get_available(tier)
            for ep in eps:
                available_llms.append(LLM(
                    base_url=f"http://{ep.ip}:{ep.port}/v1",
                    model=ep.name,
                ))
        
        if not available_llms:
            available_llms = [LLM(base_url="http://192.168.5.100:51801/v1")]
        
        def review_with_role(role: Role, llm: LLM) -> list[str]:
            messages = [
                {"role": "system", "content": role.perspective_prompt},
                {"role": "user", "content": f"""Review this from your perspective as {role.title}:

{content[:4000]}

Questions to answer:
{chr(10).join(f'- {q}' for q in role.review_questions)}

List any issues found. If everything looks good, say "No issues."
Be specific — file names, line references, exact problems."""},
            ]
            try:
                response = llm.call(messages)
                result = response.get("content", "")
                if result and "no issues" not in result.lower()[:50]:
                    return [f"[{role.title}] {result[:500]}"]
            except Exception:
                pass
            return []
        
        # Run all roles in parallel
        with ThreadPoolExecutor(max_workers=min(len(roles), len(available_llms))) as executor:
            futures = {}
            for i, role in enumerate(roles):
                llm = available_llms[i % len(available_llms)]
                future = executor.submit(review_with_role, role, llm)
                futures[future] = role.name
            
            for future in as_completed(futures):
                role_name = futures[future]
                try:
                    issues = future.result()
                    all_issues.extend(issues)
                    if issues:
                        print(f"    ⚠️ {role_name}: found issues", flush=True)
                    else:
                        print(f"    ✅ {role_name}: no issues", flush=True)
                except Exception as e:
                    print(f"    ❌ {role_name}: error — {e}", flush=True)
        
        return all_issues
    
    # ─── Step 4: Build ──────────────────────────────
    
    def _build(self, description: str, context: dict, plan: str) -> dict:
        """Build using Intern→Junior→Senior→Architect seniority chain."""
        from .seniority import SeniorityPipeline
        
        tech_stack = context.get("tech_stack", {})
        
        full_description = (
            f"{description}\n\n"
            f"Plan:\n{plan[:1500]}\n\n"
            f"Relevant files:\n{context.get('relevant_files', '')[:1000]}"
        )
        
        seniority = SeniorityPipeline(tools=self.tools, router=self.router)
        result = seniority.execute(full_description, tech_stack=tech_stack)
        
        return result
    
    # ─── Step 6: Dependency Management ──────────────
    
    def _create_prerequisite_tickets(self, parent_id: str, issues: list) -> list[str]:
        """Create prerequisite tickets from review findings."""
        created = []
        for issue in issues[:3]:
            result = self.mc._request("POST", "/api/work-items", {
                "title": f"[Prerequisite] {issue[:60]}",
                "description": issue,
                "status": "todo",
                "priority": "high",
                "parent_id": parent_id,
            })
            if "error" not in result:
                created.append(issue[:60])
        return created
    
    def _unblock_dependents(self, completed_ticket_id: str) -> list[str]:
        """Find and unblock tickets that were waiting on this one."""
        all_tickets = self.mc.get_tickets(status="blocked")
        unblocked = []
        
        for t in all_tickets:
            desc = t.get("description", "")
            if completed_ticket_id in desc or t.get("parent_id") == completed_ticket_id:
                self.mc.update_ticket(t["id"], "todo")
                unblocked.append(t["title"][:60])
        
        return unblocked
    
    def _detect_task_type(self, description: str) -> str:
        """Detect task type from description."""
        desc_lower = description.lower()
        if any(k in desc_lower for k in ["handler", "endpoint", "api", "route"]):
            return "rust_handler"
        if any(k in desc_lower for k in ["page", "component", "dashboard", "ui"]):
            return "typescript_page"
        if any(k in desc_lower for k in ["model", "struct", "schema"]):
            return "rust_model"
        if any(k in desc_lower for k in ["migration", "table", "database"]):
            return "migration"
        if any(k in desc_lower for k in ["test", "spec"]):
            return "test_writing"
        return "general"
    
    # ─── Tools ──────────────────────────────────────
    
    def _build_tools(self) -> list:
        """Build file tools scoped to the repo."""
        repo = self.repo_dir
        import subprocess
        
        def rf(filepath=""):
            f = os.path.join(repo, filepath)
            if not os.path.exists(f): return f"Not found: {filepath}"
            c = open(f).read(); return c[:4000] if len(c) > 4000 else c
        
        def lf(directory=".", pattern=""):
            full = os.path.join(repo, directory)
            exclude = {"target", "node_modules", ".git", "dist", ".next", "__pycache__"}
            files = []
            for root, dirs, fnames in os.walk(full):
                dirs[:] = [d for d in dirs if d not in exclude]
                for f in fnames:
                    if pattern and not f.endswith(pattern): continue
                    files.append(os.path.relpath(os.path.join(root, f), repo))
                if len(files) > 30: break
            return "\n".join(files[:30])
        
        def wf(filepath="", content=""):
            f = os.path.join(repo, filepath)
            os.makedirs(os.path.dirname(f), exist_ok=True)
            open(f, "w").write(content)
            return f"WRITTEN: {filepath} ({len(content)} chars)"
        
        def rc(command=""):
            try:
                r = subprocess.run(command, shell=True, capture_output=True, text=True, timeout=60, cwd=repo)
                return (r.stdout + r.stderr)[:3000]
            except Exception as e: return str(e)
        
        return [
            Tool(name="read_file", description="Read a file",
                 parameters={"type": "object", "properties": {"filepath": {"type": "string"}}, "required": ["filepath"]}, func=rf),
            Tool(name="list_files", description="List files",
                 parameters={"type": "object", "properties": {"directory": {"type": "string"}, "pattern": {"type": "string"}}}, func=lf),
            Tool(name="write_file", description="Create/overwrite a file",
                 parameters={"type": "object", "properties": {"filepath": {"type": "string"}, "content": {"type": "string"}}, "required": ["filepath", "content"]}, func=wf),
            Tool(name="run_command", description="Run shell command",
                 parameters={"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}, func=rc),
        ]
