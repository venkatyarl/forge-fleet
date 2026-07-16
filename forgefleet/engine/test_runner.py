"""Test Runner — run and parse test results for verification.

Supports: cargo test (Rust), npm test (Node.js), pytest (Python).
Used by the coding crew to verify their changes actually work.
"""
import os
import re
import subprocess
from dataclasses import dataclass, field


@dataclass
class TestResult:
    """Result of a test run."""
    framework: str  # "cargo", "npm", "pytest"
    passed: int = 0
    failed: int = 0
    skipped: int = 0
    errors: list = field(default_factory=list)  # Failed test names + messages
    duration_seconds: float = 0
    output: str = ""
    success: bool = False


class TestRunner:
    """Run tests and parse results.
    
    Auto-detects the project type from files present:
    - Cargo.toml → cargo test
    - package.json → npm test
    - pytest.ini / pyproject.toml → pytest
    """
    
    def __init__(self, repo_dir: str):
        self.repo_dir = repo_dir
    
    def detect_framework(self) -> str:
        """Detect which test framework to use."""
        if os.path.exists(os.path.join(self.repo_dir, "Cargo.toml")):
            return "cargo"
        if os.path.exists(os.path.join(self.repo_dir, "package.json")):
            return "npm"
        if os.path.exists(os.path.join(self.repo_dir, "pytest.ini")) or \
           os.path.exists(os.path.join(self.repo_dir, "pyproject.toml")):
            return "pytest"
        return "unknown"
    
    def run(self, framework: str = None, specific_test: str = None,
            timeout: int = 300) -> TestResult:
        """Run tests and return parsed results."""
        if framework is None:
            framework = self.detect_framework()
        
        if framework == "cargo":
            return self._run_cargo(specific_test, timeout)
        elif framework == "npm":
            return self._run_npm(specific_test, timeout)
        elif framework == "pytest":
            return self._run_pytest(specific_test, timeout)
        
        return TestResult(framework="unknown", errors=["No test framework detected"])
    
    def check_compiles(self) -> TestResult:
        """Just check if the project compiles (no tests)."""
        framework = self.detect_framework()
        
        if framework == "cargo":
            return self._run_command("cargo", ["cargo", "check"], 120)
        elif framework == "npm":
            return self._run_command("npm", ["npm", "run", "build"], 120)
        
        return TestResult(framework=framework, success=True)
    
    def _run_cargo(self, specific_test: str, timeout: int) -> TestResult:
        """Run cargo test and parse results."""
        cmd = ["cargo", "test", "--", "--color=never"]
        if specific_test:
            cmd.insert(2, specific_test)
        
        result = self._run_command("cargo", cmd, timeout)
        
        # Parse cargo test output
        output = result.output
        
        # Look for "test result: ok. X passed; Y failed; Z ignored"
        match = re.search(
            r'test result: \w+\. (\d+) passed; (\d+) failed; (\d+) (?:ignored|filtered)',
            output,
        )
        if match:
            result.passed = int(match.group(1))
            result.failed = int(match.group(2))
            result.skipped = int(match.group(3))
            result.success = result.failed == 0
        
        # Extract failed test names
        for match in re.finditer(r'---- (\S+) stdout ----\n(.*?)(?=\n----|\nfailures)', output, re.DOTALL):
            result.errors.append({
                "test": match.group(1),
                "output": match.group(2)[:500],
            })
        
        return result
    
    def _run_npm(self, specific_test: str, timeout: int) -> TestResult:
        """Run npm test and parse results."""
        cmd = ["npm", "test"]
        if specific_test:
            cmd.extend(["--", specific_test])
        
        result = self._run_command("npm", cmd, timeout)
        
        # Parse common test frameworks (Jest, Vitest)
        output = result.output
        
        # Jest/Vitest: "Tests: X passed, Y failed"
        match = re.search(r'Tests:\s+(\d+) passed(?:,\s+(\d+) failed)?', output)
        if match:
            result.passed = int(match.group(1))
            result.failed = int(match.group(2) or 0)
            result.success = result.failed == 0
        
        return result
    
    def _run_pytest(self, specific_test: str, timeout: int) -> TestResult:
        """Run pytest and parse results."""
        cmd = ["python3", "-m", "pytest", "-v"]
        if specific_test:
            cmd.append(specific_test)
        
        result = self._run_command("pytest", cmd, timeout)
        
        # Parse pytest output: "X passed, Y failed"
        output = result.output
        match = re.search(r'(\d+) passed(?:.*?(\d+) failed)?', output)
        if match:
            result.passed = int(match.group(1))
            result.failed = int(match.group(2) or 0)
            result.success = result.failed == 0
        
        return result
    
    def _run_command(self, framework: str, cmd: list, timeout: int) -> TestResult:
        """Run a command and capture output."""
        import time
        start = time.time()
        
        try:
            r = subprocess.run(
                cmd, capture_output=True, text=True,
                timeout=timeout, cwd=self.repo_dir,
            )
            duration = time.time() - start
            output = r.stdout + r.stderr
            
            if len(output) > 10000:
                output = output[:5000] + "\n...[truncated]...\n" + output[-5000:]
            
            return TestResult(
                framework=framework,
                success=r.returncode == 0,
                duration_seconds=round(duration, 1),
                output=output,
            )
        except subprocess.TimeoutExpired:
            return TestResult(
                framework=framework, success=False,
                errors=[{"test": "timeout", "output": f"Tests timed out after {timeout}s"}],
                output=f"Timeout after {timeout}s",
            )
        except Exception as e:
            return TestResult(
                framework=framework, success=False,
                errors=[{"test": "error", "output": str(e)}],
            )
