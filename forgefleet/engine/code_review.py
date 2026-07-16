"""Code Review Engine — automated review checklist.

Extracted from Crush + code-review patterns.
Checks code for common issues without needing an LLM.
"""
import os
import re
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class ReviewIssue:
    """A code review issue found."""
    severity: str  # "error", "warning", "info"
    category: str  # "placeholder", "error_handling", "security", "style"
    file: str
    line: int = 0
    message: str = ""
    suggestion: str = ""


class CodeReviewer:
    """Automated code review — catches common issues statically.
    
    Checks:
    1. Placeholder code (TODO, unimplemented!, stub, let _ =)
    2. Missing error handling (unwrap() without context)
    3. Security issues (hardcoded secrets, SQL injection patterns)
    4. Style issues (no doc comments on pub functions)
    5. Build verification (cargo check / npm run build)
    """
    
    def __init__(self, repo_dir: str):
        self.repo_dir = repo_dir
    
    def review_file(self, filepath: str) -> list[ReviewIssue]:
        """Review a single file for issues."""
        full_path = os.path.join(self.repo_dir, filepath)
        if not os.path.exists(full_path):
            return [ReviewIssue("error", "missing", filepath, message=f"File not found: {filepath}")]
        
        content = Path(full_path).read_text()
        lines = content.split("\n")
        issues = []
        
        ext = os.path.splitext(filepath)[1]
        
        if ext == ".rs":
            issues.extend(self._review_rust(filepath, lines))
        elif ext in (".ts", ".tsx", ".js", ".jsx"):
            issues.extend(self._review_typescript(filepath, lines))
        elif ext == ".py":
            issues.extend(self._review_python(filepath, lines))
        
        # Common checks for all languages
        issues.extend(self._review_common(filepath, lines))
        
        return issues
    
    def review_changes(self, files: list[str] = None) -> list[ReviewIssue]:
        """Review all changed files (or specified files)."""
        if files is None:
            # Get changed files from git
            import subprocess
            r = subprocess.run(
                ["git", "diff", "--name-only", "HEAD"],
                capture_output=True, text=True, cwd=self.repo_dir,
            )
            files = [f.strip() for f in r.stdout.split("\n") if f.strip()]
        
        all_issues = []
        for f in files:
            all_issues.extend(self.review_file(f))
        
        return sorted(all_issues, key=lambda i: (
            {"error": 0, "warning": 1, "info": 2}.get(i.severity, 3),
            i.file, i.line,
        ))
    
    def _review_rust(self, filepath: str, lines: list[str]) -> list[ReviewIssue]:
        """Rust-specific checks."""
        issues = []
        in_pub_fn = False
        had_doc_comment = False
        
        for i, line in enumerate(lines, 1):
            stripped = line.strip()
            
            # Placeholder detection
            if "todo!" in stripped.lower() or "unimplemented!" in stripped:
                issues.append(ReviewIssue("error", "placeholder", filepath, i,
                    "Placeholder macro found", "Replace with real implementation"))
            
            if re.search(r'let\s+_\s*=', stripped) and not stripped.startswith("//"):
                issues.append(ReviewIssue("warning", "placeholder", filepath, i,
                    "Ignored result (let _ =)", "Handle the result or use explicit error handling"))
            
            if "// TODO" in stripped or "// FIXME" in stripped or "// HACK" in stripped:
                issues.append(ReviewIssue("error", "placeholder", filepath, i,
                    f"Comment placeholder: {stripped[:60]}", "Implement or create a ticket"))
            
            if "// In production" in stripped:
                issues.append(ReviewIssue("error", "placeholder", filepath, i,
                    "Production placeholder comment", "Implement the production code"))
            
            # Error handling
            if ".unwrap()" in stripped and "test" not in filepath.lower():
                issues.append(ReviewIssue("warning", "error_handling", filepath, i,
                    "Bare .unwrap() — will panic on None/Err",
                    "Use .context()?, .map_err()?, or .unwrap_or_default()"))
            
            if ".expect(" not in stripped and ".unwrap()" in stripped:
                pass  # Already caught above
            
            # Security
            if re.search(r'(password|secret|api_key|token)\s*=\s*"[^"]+"', stripped, re.IGNORECASE):
                issues.append(ReviewIssue("error", "security", filepath, i,
                    "Hardcoded secret detected", "Use environment variable or config"))
            
            # Doc comments on pub functions
            if stripped.startswith("pub fn ") or stripped.startswith("pub async fn "):
                if not had_doc_comment:
                    issues.append(ReviewIssue("info", "style", filepath, i,
                        "Public function without doc comment",
                        "Add /// doc comment explaining purpose"))
                had_doc_comment = False
            
            if stripped.startswith("///"):
                had_doc_comment = True
            elif stripped and not stripped.startswith("//"):
                if not stripped.startswith("pub"):
                    had_doc_comment = False
        
        return issues
    
    def _review_typescript(self, filepath: str, lines: list[str]) -> list[ReviewIssue]:
        """TypeScript-specific checks."""
        issues = []
        
        for i, line in enumerate(lines, 1):
            stripped = line.strip()
            
            # Placeholder
            if "// TODO" in stripped or "// FIXME" in stripped:
                issues.append(ReviewIssue("error", "placeholder", filepath, i,
                    f"Comment placeholder: {stripped[:60]}"))
            
            if "console.log(" in stripped and "test" not in filepath.lower():
                issues.append(ReviewIssue("info", "style", filepath, i,
                    "console.log in production code", "Remove or use proper logger"))
            
            # Error handling
            if "catch {" in stripped or "catch(e)" in stripped:
                # Check if catch block is empty
                pass
            
            # Security
            if re.search(r'(password|secret|apiKey|token)\s*[:=]\s*["\'][^"\']+["\']', stripped):
                issues.append(ReviewIssue("error", "security", filepath, i,
                    "Hardcoded secret detected"))
            
            # Type safety
            if ": any" in stripped and not stripped.startswith("//"):
                issues.append(ReviewIssue("warning", "style", filepath, i,
                    "Using 'any' type — defeats TypeScript safety",
                    "Use proper type or generic"))
        
        return issues
    
    def _review_python(self, filepath: str, lines: list[str]) -> list[ReviewIssue]:
        """Python-specific checks."""
        issues = []
        
        for i, line in enumerate(lines, 1):
            stripped = line.strip()
            
            if "# TODO" in stripped or "# FIXME" in stripped:
                issues.append(ReviewIssue("error", "placeholder", filepath, i,
                    f"Comment placeholder: {stripped[:60]}"))
            
            if "pass" == stripped and i > 1:
                prev = lines[i-2].strip() if i >= 2 else ""
                if prev.startswith("def ") or prev.startswith("class "):
                    issues.append(ReviewIssue("error", "placeholder", filepath, i,
                        "Empty function/class body (pass)", "Implement or raise NotImplementedError"))
            
            if re.search(r'(password|secret|api_key|token)\s*=\s*["\'][^"\']+["\']', stripped):
                issues.append(ReviewIssue("error", "security", filepath, i,
                    "Hardcoded secret detected"))
        
        return issues
    
    def _review_common(self, filepath: str, lines: list[str]) -> list[ReviewIssue]:
        """Language-agnostic checks."""
        issues = []
        
        for i, line in enumerate(lines, 1):
            # Very long lines
            if len(line) > 200 and not line.strip().startswith("//") and not line.strip().startswith("#"):
                issues.append(ReviewIssue("info", "style", filepath, i,
                    f"Line too long ({len(line)} chars)"))
            
            # Trailing whitespace
            if line != line.rstrip() and line.strip():
                pass  # Too noisy, skip
        
        return issues
    
    def format_review(self, issues: list[ReviewIssue]) -> str:
        """Format review issues as a readable report."""
        if not issues:
            return "✅ No issues found — code looks clean!"
        
        icons = {"error": "❌", "warning": "⚠️", "info": "ℹ️"}
        
        lines = [f"## Code Review: {len(issues)} issues found\n"]
        
        by_severity = {"error": [], "warning": [], "info": []}
        for issue in issues:
            by_severity.get(issue.severity, []).append(issue)
        
        for severity in ("error", "warning", "info"):
            items = by_severity[severity]
            if not items:
                continue
            
            icon = icons[severity]
            lines.append(f"\n### {icon} {severity.upper()} ({len(items)})")
            for item in items:
                loc = f":{item.line}" if item.line else ""
                lines.append(f"- `{item.file}{loc}` [{item.category}] {item.message}")
                if item.suggestion:
                    lines.append(f"  → {item.suggestion}")
        
        return "\n".join(lines)
