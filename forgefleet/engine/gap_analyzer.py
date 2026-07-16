"""Gap Analyzer + Improvement Suggester — find what's missing and what could be better.

Two capabilities:
1. Gap Analysis: compare task requirements vs actual code → find missing pieces
2. Improvement Suggestions: review working code → suggest enhancements

These run AFTER a task is "done" to catch missed requirements and raise quality.
"""
import os
from dataclasses import dataclass, field
from .llm import LLM
from .tool import Tool


@dataclass
class Gap:
    """A gap between requirements and implementation."""
    category: str  # "missing_feature", "incomplete", "wrong_behavior", "missing_test"
    description: str
    severity: str  # "critical", "major", "minor"
    suggested_fix: str = ""
    file: str = ""


@dataclass
class Improvement:
    """A suggested improvement to working code."""
    category: str  # "performance", "security", "ux", "reliability", "maintainability"
    description: str
    effort: str  # "trivial", "small", "medium", "large"
    impact: str  # "high", "medium", "low"
    code_suggestion: str = ""
    file: str = ""


class GapAnalyzer:
    """Find gaps between what was asked and what was built.
    
    Uses an LLM to compare task requirements against actual code,
    identifying missing features, incomplete implementations, and wrong behavior.
    """
    
    def __init__(self, llm: LLM = None, repo_dir: str = ""):
        self.llm = llm or LLM(base_url="http://192.168.5.100:51801/v1")  # 72B for analysis
        self.repo_dir = repo_dir
    
    def analyze_gaps(self, task_description: str, code_output: str,
                     files_changed: list = None) -> list[Gap]:
        """Compare task requirements vs actual output."""
        
        # Read the actual files if paths provided
        file_contents = ""
        if files_changed and self.repo_dir:
            for fp in files_changed[:5]:
                full = os.path.join(self.repo_dir, fp)
                if os.path.exists(full):
                    content = open(full).read()
                    file_contents += f"\n### {fp}\n```\n{content[:3000]}\n```\n"
        
        messages = [
            {"role": "system", "content": """You are a QA lead doing gap analysis.
Compare the TASK REQUIREMENTS against the ACTUAL CODE.
Find what's MISSING, INCOMPLETE, or WRONG.

Output as JSON array:
[{"category": "missing_feature|incomplete|wrong_behavior|missing_test",
  "description": "what's missing",
  "severity": "critical|major|minor",
  "suggested_fix": "how to fix",
  "file": "which file"}]

Be thorough but practical. Don't flag style issues — only real gaps.
If everything looks good, return an empty array: []"""},
            {"role": "user", "content": f"""TASK REQUIREMENTS:
{task_description}

ACTUAL CODE OUTPUT:
{code_output[:4000]}

{f"FILES CHANGED:{file_contents}" if file_contents else ""}

Find gaps between requirements and implementation. Output JSON array only."""},
        ]
        
        try:
            response = self.llm.call(messages)
            content = response.get("content", "[]")
            return self._parse_gaps(content)
        except Exception as e:
            return [Gap("error", f"Gap analysis failed: {e}", "minor")]
    
    def suggest_improvements(self, code: str, context: str = "") -> list[Improvement]:
        """Review working code and suggest improvements.
        
        Categories: performance, security, UX, reliability, maintainability.
        """
        messages = [
            {"role": "system", "content": """You are a senior architect reviewing code for improvements.
The code WORKS — you're looking for ways to make it BETTER.

Categories:
- performance: caching, pagination, async, indexing
- security: input validation, rate limiting, auth checks
- ux: loading states, error messages, accessibility
- reliability: error handling, retries, circuit breakers
- maintainability: abstractions, documentation, tests

Output as JSON array:
[{"category": "performance|security|ux|reliability|maintainability",
  "description": "what could be better",
  "effort": "trivial|small|medium|large",
  "impact": "high|medium|low",
  "code_suggestion": "brief code example if applicable",
  "file": "which file"}]

Only suggest practical improvements. Max 5 suggestions.
If code is already solid, return fewer items."""},
            {"role": "user", "content": f"""CODE TO REVIEW:
{code[:6000]}

{f"CONTEXT: {context}" if context else ""}

Suggest improvements. Output JSON array only."""},
        ]
        
        try:
            response = self.llm.call(messages)
            content = response.get("content", "[]")
            return self._parse_improvements(content)
        except Exception as e:
            return [Improvement("error", f"Analysis failed: {e}", "trivial", "low")]
    
    def full_review(self, task: str, code: str, files: list = None) -> dict:
        """Complete review: gaps + improvements in one call."""
        gaps = self.analyze_gaps(task, code, files)
        improvements = self.suggest_improvements(code, task)
        
        critical_gaps = [g for g in gaps if g.severity == "critical"]
        major_gaps = [g for g in gaps if g.severity == "major"]
        high_impact = [i for i in improvements if i.impact == "high"]
        
        return {
            "gaps": {
                "total": len(gaps),
                "critical": len(critical_gaps),
                "major": len(major_gaps),
                "items": [{"cat": g.category, "desc": g.description, "sev": g.severity} for g in gaps],
            },
            "improvements": {
                "total": len(improvements),
                "high_impact": len(high_impact),
                "items": [{"cat": i.category, "desc": i.description, "effort": i.effort, "impact": i.impact} for i in improvements],
            },
            "verdict": "NEEDS_WORK" if critical_gaps else ("GOOD_WITH_SUGGESTIONS" if gaps or improvements else "EXCELLENT"),
        }
    
    def _parse_gaps(self, text: str) -> list[Gap]:
        """Parse JSON gaps from LLM output."""
        import json, re
        
        try:
            data = json.loads(text)
        except json.JSONDecodeError:
            m = re.search(r'\[.*\]', text, re.DOTALL)
            if m:
                try:
                    data = json.loads(m.group())
                except:
                    return []
            else:
                return []
        
        return [
            Gap(
                category=g.get("category", "unknown"),
                description=g.get("description", ""),
                severity=g.get("severity", "minor"),
                suggested_fix=g.get("suggested_fix", ""),
                file=g.get("file", ""),
            )
            for g in data if isinstance(g, dict)
        ]
    
    def _parse_improvements(self, text: str) -> list[Improvement]:
        """Parse JSON improvements from LLM output."""
        import json, re
        
        try:
            data = json.loads(text)
        except json.JSONDecodeError:
            m = re.search(r'\[.*\]', text, re.DOTALL)
            if m:
                try:
                    data = json.loads(m.group())
                except:
                    return []
            else:
                return []
        
        return [
            Improvement(
                category=i.get("category", "unknown"),
                description=i.get("description", ""),
                effort=i.get("effort", "medium"),
                impact=i.get("impact", "medium"),
                code_suggestion=i.get("code_suggestion", ""),
                file=i.get("file", ""),
            )
            for i in data if isinstance(i, dict)
        ]
    
    def format_report(self, review: dict) -> str:
        """Format a full review as readable text."""
        lines = [f"## Review: {review['verdict']}\n"]
        
        gaps = review["gaps"]
        if gaps["total"]:
            lines.append(f"### Gaps ({gaps['total']})")
            for g in gaps["items"]:
                icon = {"critical": "🔴", "major": "🟡", "minor": "🔵"}.get(g["sev"], "⚪")
                lines.append(f"  {icon} [{g['cat']}] {g['desc']}")
        
        improvements = review["improvements"]
        if improvements["total"]:
            lines.append(f"\n### Improvements ({improvements['total']})")
            for i in improvements["items"]:
                impact_icon = {"high": "🔥", "medium": "💡", "low": "ℹ️"}.get(i["impact"], "")
                lines.append(f"  {impact_icon} [{i['cat']}] {i['desc']} (effort: {i['effort']})")
        
        return "\n".join(lines)
