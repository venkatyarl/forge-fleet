"""Auto-Fix Loop — write code → test → fix → retest automatically.

The #1 feature that makes Codex actually work.
Without this, agents write code that doesn't compile and give up.
With this, they iterate until it works (up to N attempts).

Flow:
1. Agent writes code
2. Run cargo check / npm build in sandbox
3. If passes → apply to real repo, done
4. If fails → feed error back to agent with "fix this"
5. Agent reads error, writes fix
6. Goto 2 (up to max_attempts)
"""
import time
from dataclasses import dataclass, field
from typing import Optional
from .llm import LLM
from .sandbox import DockerSandbox, SandboxResult
from .test_runner import TestRunner, TestResult
from .diff_editor import DiffEditor, EditResult
from .self_improve import SelfImprover, Learning


@dataclass
class FixAttempt:
    """One attempt in the auto-fix loop."""
    attempt: int
    action: str  # "initial", "fix"
    changes: dict = field(default_factory=dict)  # filepath -> content
    test_result: str = ""
    success: bool = False
    error: str = ""
    duration: float = 0


@dataclass
class AutoFixResult:
    """Result of the entire auto-fix loop."""
    success: bool
    attempts: list = field(default_factory=list)
    final_changes: dict = field(default_factory=dict)
    total_duration: float = 0
    model_used: str = ""


class AutoFixer:
    """Iterative code → test → fix → retest loop.
    
    Like Codex: writes code, tests it, reads errors, fixes them.
    Keeps going until it works or gives up.
    
    Can use Docker sandbox (safe) or direct testing (faster).
    """
    
    def __init__(self, repo_dir: str, llm: LLM, 
                 max_attempts: int = 3, use_sandbox: bool = True):
        self.repo_dir = repo_dir
        self.llm = llm
        self.max_attempts = max_attempts
        self.use_sandbox = use_sandbox and DockerSandbox.is_available()
        self.diff_editor = DiffEditor(repo_dir)
        self.test_runner = TestRunner(repo_dir)
        self.sandbox = DockerSandbox(repo_dir) if self.use_sandbox else None
        self.improver = SelfImprover()
    
    def run(self, task: str, initial_code: str = "") -> AutoFixResult:
        """Run the auto-fix loop.
        
        Args:
            task: description of what to build
            initial_code: LLM's first attempt (if already generated)
            
        Returns:
            AutoFixResult with all attempts and final changes
        """
        start = time.time()
        result = AutoFixResult(success=False, model_used=self.llm.model)
        
        # Track accumulated changes
        all_changes = {}
        last_error = ""
        
        for attempt_num in range(1, self.max_attempts + 1):
            attempt = FixAttempt(
                attempt=attempt_num,
                action="initial" if attempt_num == 1 else "fix",
            )
            attempt_start = time.time()
            
            try:
                # Step 1: Get code from LLM
                if attempt_num == 1 and initial_code:
                    llm_output = initial_code
                elif attempt_num == 1:
                    llm_output = self._generate_initial(task)
                else:
                    llm_output = self._generate_fix(task, last_error, all_changes)
                
                # Step 2: Parse LLM output into file changes
                edits = self.diff_editor.apply_llm_output(llm_output)
                
                new_changes = {}
                for edit in edits:
                    if edit.success:
                        filepath = edit.filepath
                        full_path = f"{self.repo_dir}/{filepath}"
                        if os.path.exists(full_path):
                            new_changes[filepath] = open(full_path).read()
                
                all_changes.update(new_changes)
                attempt.changes = new_changes
                
                # Step 3: Test the changes
                if self.use_sandbox and self.sandbox:
                    sandbox_result = self.sandbox.test_changes(all_changes)
                    test_passed = sandbox_result.success
                    test_output = sandbox_result.stdout + sandbox_result.stderr
                else:
                    # Direct testing (no sandbox)
                    test_result = self.test_runner.check_compiles()
                    test_passed = test_result.success
                    test_output = test_result.output
                
                attempt.test_result = test_output[:2000]
                attempt.success = test_passed
                attempt.duration = time.time() - attempt_start
                
                if test_passed:
                    result.success = True
                    result.final_changes = all_changes
                    result.attempts.append(attempt)
                    
                    # Record success for self-improvement
                    self.improver.record(Learning(
                        task_type="auto_fix",
                        model_used=self.llm.model,
                        tier=0,
                        outcome="success",
                        duration_seconds=time.time() - start,
                    ))
                    break
                else:
                    last_error = test_output[:3000]
                    attempt.error = last_error
                    
                    # Check if we've seen this error before
                    known_fix = self.improver.get_known_fix(last_error[:100])
                    if known_fix:
                        last_error += f"\n\nKnown fix from previous experience: {known_fix}"
                
            except Exception as e:
                attempt.error = str(e)
                attempt.duration = time.time() - attempt_start
                last_error = str(e)
            
            result.attempts.append(attempt)
        
        # If all attempts failed, record failure
        if not result.success:
            self.improver.record(Learning(
                task_type="auto_fix",
                model_used=self.llm.model,
                tier=0,
                outcome="failure",
                error_pattern=last_error[:200] if last_error else "unknown",
                duration_seconds=time.time() - start,
            ))
        
        result.total_duration = time.time() - start
        return result
    
    def _generate_initial(self, task: str) -> str:
        """Generate initial code attempt."""
        messages = [
            {"role": "system", "content": (
                "You are a senior developer. Write COMPLETE, production-ready code. "
                "Output your changes as file blocks:\n"
                "```filepath.rs\ncomplete file content\n```\n"
                "Or use search/replace blocks:\n"
                "filepath\n<<<<<<< SEARCH\nold code\n=======\nnew code\n>>>>>>> REPLACE\n"
                "NEVER use TODO, unimplemented!, or placeholder code."
            )},
            {"role": "user", "content": task},
        ]
        
        response = self.llm.call(messages)
        return response.get("content", "")
    
    def _generate_fix(self, task: str, error: str, current_changes: dict) -> str:
        """Generate a fix based on the error."""
        changes_summary = "\n".join(
            f"- {fp} ({len(content)} chars)"
            for fp, content in current_changes.items()
        )
        
        messages = [
            {"role": "system", "content": (
                "You are fixing code that failed to compile/test. "
                "Read the error carefully, identify the root cause, and provide the fix. "
                "Output ONLY the changed files using the same format as before."
            )},
            {"role": "user", "content": (
                f"Task: {task}\n\n"
                f"Files changed so far:\n{changes_summary}\n\n"
                f"Error from build/test:\n```\n{error}\n```\n\n"
                f"Fix the error. Output the corrected file(s)."
            )},
        ]
        
        response = self.llm.call(messages)
        return response.get("content", "")


# Need os import for file operations
import os
