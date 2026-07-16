"""Output Validator — verify LLM output before accepting.

Checks output against template rules and banned patterns.
Rejects bad output so the auto-fix loop can retry.
"""
import re
from dataclasses import dataclass, field
from .prompt_templates import PromptTemplate


@dataclass
class ValidationResult:
    """Result of validating LLM output."""
    valid: bool
    issues: list = field(default_factory=list)
    score: float = 1.0  # 0-1, how good is the output


class OutputValidator:
    """Validate LLM output against rules before accepting."""
    
    def validate(self, output: str, template: PromptTemplate = None) -> ValidationResult:
        """Validate output against template rules."""
        issues = []
        
        if not output or len(output.strip()) < 10:
            return ValidationResult(valid=False, issues=["Empty or too short output"], score=0)
        
        # Check banned patterns
        if template:
            for pattern in template.banned_patterns:
                if pattern in output:
                    count = output.count(pattern)
                    issues.append(f"Banned pattern '{pattern}' found {count}x")
        
        # Generic checks
        issues.extend(self._check_placeholders(output))
        issues.extend(self._check_code_quality(output))
        
        # Calculate score
        score = 1.0 - (len(issues) * 0.15)
        score = max(0.0, min(1.0, score))
        
        # Valid if score > 0.5 and no critical issues
        critical = any("Banned pattern" in i for i in issues)
        valid = score > 0.5 and not critical
        
        return ValidationResult(valid=valid, issues=issues, score=score)
    
    def _check_placeholders(self, output: str) -> list[str]:
        """Check for placeholder code that shouldn't ship."""
        issues = []
        patterns = {
            r'todo!': "Rust todo! macro",
            r'unimplemented!': "Rust unimplemented! macro",
            r'// TODO': "TODO comment",
            r'// FIXME': "FIXME comment",
            r'// HACK': "HACK comment",
            r'# TODO': "Python TODO",
            r'pass\s*$': "Python empty pass",
            r'// In production': "Production placeholder",
            r'let _ =': "Ignored result",
            r'\.unwrap\(\)': "Bare unwrap()",
        }
        
        for pattern, desc in patterns.items():
            if re.search(pattern, output, re.MULTILINE):
                issues.append(f"Placeholder: {desc}")
        
        return issues
    
    def _check_code_quality(self, output: str) -> list[str]:
        """Check basic code quality."""
        issues = []
        
        # Check for overly long lines
        lines = output.split("\n")
        long_lines = sum(1 for l in lines if len(l) > 200 and not l.strip().startswith("//"))
        if long_lines > 5:
            issues.append(f"{long_lines} lines over 200 chars")
        
        # Check for empty error handling
        if re.search(r'catch\s*\{?\s*\}', output):
            issues.append("Empty catch block")
        
        # Check for hardcoded secrets
        if re.search(r'(password|secret|api_key|token)\s*=\s*"[^"]{8,}"', output, re.IGNORECASE):
            issues.append("Possible hardcoded secret")
        
        return issues
    
    def format_issues(self, result: ValidationResult) -> str:
        """Format validation issues for feedback to LLM."""
        if result.valid:
            return "✅ Output passes validation"
        
        lines = [f"❌ Output rejected (score: {result.score:.0%}). Issues:"]
        for issue in result.issues:
            lines.append(f"  - {issue}")
        lines.append("\nPlease fix these issues and regenerate.")
        return "\n".join(lines)
