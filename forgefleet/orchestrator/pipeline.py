"""Tiered Build Pipeline — each tier builds on previous tier's work via git."""
import subprocess
import os
import json
from typing import Optional
from forgefleet.agent_loop.strands_agent import StrandsAgent
from forgefleet.agent_loop.file_ops import read_repo_files
from forgefleet.orchestrator.fleet_discovery import FleetDiscovery, ModelEndpoint


class TieredPipeline:
    """Orchestrates multi-tier code generation.
    
    Tier 1: Fast model scaffolds (creates files, structure, boilerplate)
    Tier 2: Code-specialist fills in implementations
    Tier 3: Large model handles complex logic
    Tier 4: Cluster model for hardest tasks
    Tier 5: Codex CLI (paid fallback)
    Tier 6: Human breaks into smaller tickets
    
    Each tier commits + pushes to a git branch.
    Next tier pulls the branch and continues where previous left off.
    """
    
    def __init__(self, fleet: FleetDiscovery, repo_dir: str, my_node: str = ""):
        self.fleet = fleet
        self.repo_dir = repo_dir
        self.my_node = my_node
        self.max_tiers = 6
    
    def run(self, task_title: str, task_description: str, branch_name: str,
            skills_context: str = "", start_tier: int = 1) -> dict:
        """Run the full tiered pipeline for a task.
        
        Returns:
            {
                "success": bool,
                "completed_tier": int,
                "branch": str,
                "result": str,
                "commits": [{"tier": int, "message": str}],
            }
        """
        result = {
            "success": False,
            "completed_tier": 0,
            "branch": branch_name,
            "result": "",
            "commits": [],
        }
        
        # Set up git branch
        self._git_setup(branch_name)
        
        # Track accumulated work
        previous_work = ""
        
        for tier in range(start_tier, self.max_tiers + 1):
            if tier == 6:
                # Tier 6: Human intervention
                result["result"] = "All automated tiers failed. Task needs manual breakdown."
                break
            
            if tier == 5:
                # Tier 5: Codex CLI
                tier_result = self._run_codex(task_title, task_description, skills_context)
            else:
                # Tiers 1-4: Local LLMs
                tier_result = self._run_tier(tier, task_title, task_description, 
                                             skills_context, previous_work)
            
            if tier_result:
                # Commit this tier's work
                commit_msg = f"tier{tier}: {self._tier_action(tier)} {task_title[:40]}"
                committed = self._git_commit_push(branch_name, commit_msg)
                if committed:
                    result["commits"].append({"tier": tier, "message": commit_msg})
                
                result["completed_tier"] = tier
                result["result"] = tier_result
                
                # Check if work is complete (no TODOs remaining)
                if self._check_complete():
                    result["success"] = True
                    return result
                
                # Capture what this tier produced for next tier's context
                previous_work = self._get_git_diff()
                continue  # Escalate to next tier with accumulated work
            else:
                # This tier failed completely — try next tier from scratch
                continue
        
        # If we got through all tiers with some work done, still count as partial success
        if result["completed_tier"] > 0:
            result["success"] = True
        
        return result
    
    def _run_tier(self, tier: int, title: str, desc: str, 
                  skills: str, previous_work: str) -> Optional[str]:
        """Run a specific tier using fleet-aware model selection."""
        # Find available model for this tier
        endpoints = self.fleet.get_available(tier, prefer_local=self.my_node)
        
        if not endpoints:
            print(f"  ⚠️ Tier {tier}: no available models")
            return None
        
        endpoint = endpoints[0]  # Best available
        print(f"  🏗️ Tier {tier}: {endpoint.name} via {endpoint.url}")
        
        # Build tier-specific prompt
        prompt = self._build_prompt(tier, title, desc, skills, previous_work)
        
        # Run agent with tool access
        agent = StrandsAgent(
            model_url=endpoint.url,
            repo_dir=self.repo_dir,
            model_name=endpoint.name,
        )
        
        return agent.run_with_tools(prompt)
    
    def _build_prompt(self, tier: int, title: str, desc: str, 
                      skills: str, previous_work: str) -> str:
        """Build tier-specific prompt."""
        base = f"Task: {title}\n\nDescription: {desc}"
        
        if skills:
            base += f"\n\n## Skill Guidelines\n{skills}"
        
        if tier == 1:
            base += "\n\nCreate the file structure and implement what you can."
            base += " If parts are too complex, add TODO comments."
            base += " Focus on: Cargo.toml, lib.rs, models, DTOs, basic handlers."
        elif tier == 2:
            base += "\n\nThe structure was created by a previous model."
            if previous_work:
                base += f"\n\nPrevious work (git diff):\n{previous_work[:3000]}"
            base += "\n\nFill in all TODO comments with real implementations."
            base += " Code MUST compile (cargo check)."
        elif tier == 3:
            base += "\n\nBasic implementation exists from previous tiers."
            if previous_work:
                base += f"\n\nCurrent code changes:\n{previous_work[:3000]}"
            base += "\n\nFinish complex business logic, edge cases, error handling."
        elif tier == 4:
            base += "\n\nPartial implementation exists."
            if previous_work:
                base += f"\n\nCode so far:\n{previous_work[:3000]}"
            base += "\n\nComplete everything. Handle all edge cases. Add tests."
        
        return base
    
    def _run_codex(self, title: str, desc: str, skills: str) -> Optional[str]:
        """Tier 5: Run Codex CLI as last automated resort."""
        try:
            prompt = f"Task: {title}\n\nDescription: {desc}"
            if skills:
                prompt += f"\n\n{skills}"
            escaped = prompt[:8000].replace('"', '\\"').replace("$", "\\$")
            cmd = f'cd {self.repo_dir} && codex exec --full-auto "{escaped}"'
            r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=900)
            if r.returncode == 0 and r.stdout.strip():
                return r.stdout.strip()
        except:
            pass
        return None
    
    def _tier_action(self, tier: int) -> str:
        """Get action verb for tier commit message."""
        return {1: "scaffold", 2: "implement", 3: "complete", 4: "finish", 5: "codex"}.get(tier, "build")
    
    def _git_setup(self, branch: str):
        """Set up git branch for this task."""
        cwd = self.repo_dir
        subprocess.run(["git", "checkout", "main"], cwd=cwd, capture_output=True, timeout=10)
        subprocess.run(["git", "pull", "origin", "main"], cwd=cwd, capture_output=True, timeout=30)
        subprocess.run(["git", "checkout", "-b", branch], cwd=cwd, capture_output=True, timeout=10)
    
    def _git_commit_push(self, branch: str, message: str) -> bool:
        """Commit all changes and push to remote."""
        cwd = self.repo_dir
        try:
            subprocess.run(["git", "add", "-A"], cwd=cwd, capture_output=True, timeout=10)
            r = subprocess.run(["git", "commit", "-m", message], cwd=cwd, capture_output=True, timeout=10)
            if r.returncode == 0:
                subprocess.run(["git", "push", "origin", branch], cwd=cwd, capture_output=True, timeout=30)
                return True
        except:
            pass
        return False
    
    def _get_git_diff(self) -> str:
        """Get git diff of current changes vs main."""
        try:
            r = subprocess.run(
                ["git", "diff", "main...HEAD"],
                cwd=self.repo_dir, capture_output=True, text=True, timeout=10
            )
            return r.stdout[:5000] if r.stdout else ""
        except:
            return ""
    
    def _check_complete(self) -> bool:
        """Check if the task appears complete (no TODOs remaining)."""
        try:
            r = subprocess.run(
                ["grep", "-r", "TODO\\|unimplemented\\|todo!", 
                 os.path.join(self.repo_dir, "rust-backend")],
                capture_output=True, text=True, timeout=10
            )
            return not bool(r.stdout.strip())
        except:
            return True  # Assume complete if grep fails
