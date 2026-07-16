"""Skill Loader — integrate ClawHub skills into ForgeFleet agent prompts.

Reads SKILL.md files, matches to tasks by keyword, injects into system prompts.
So when ForgeFleet handles a "code review" task, it loads the code-review skill.
"""
import os
import re
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class LoadedSkill:
    """A skill loaded from SKILL.md."""
    name: str
    description: str
    content: str  # Full SKILL.md content
    triggers: list = field(default_factory=list)  # Keywords that trigger this skill
    path: str = ""


class SkillLoader:
    """Load and match ClawHub skills for ForgeFleet agents.
    
    Searches skill directories for SKILL.md files,
    extracts trigger keywords, and matches them to task descriptions.
    """
    
    SKILL_DIRS = [
        os.path.expanduser("~/.openclaw/workspace/skills"),
        os.path.expanduser("~/taylorProjects/mission-control/skills"),
    ]
    
    def __init__(self, extra_dirs: list = None):
        self.skills: dict[str, LoadedSkill] = {}
        self._load_all(extra_dirs or [])
    
    def _load_all(self, extra_dirs: list):
        """Load all skills from known directories."""
        all_dirs = self.SKILL_DIRS + extra_dirs
        
        for base_dir in all_dirs:
            if not os.path.exists(base_dir):
                continue
            
            for entry in os.listdir(base_dir):
                skill_dir = os.path.join(base_dir, entry)
                skill_file = os.path.join(skill_dir, "SKILL.md")
                
                if os.path.isfile(skill_file):
                    try:
                        content = Path(skill_file).read_text()
                        skill = self._parse_skill(entry, content, skill_file)
                        self.skills[skill.name] = skill
                    except Exception:
                        pass
    
    def _parse_skill(self, name: str, content: str, path: str) -> LoadedSkill:
        """Parse a SKILL.md file to extract metadata."""
        # Extract description from first paragraph or # heading
        description = ""
        for line in content.split("\n"):
            line = line.strip()
            if line.startswith("# "):
                description = line[2:]
                break
            if line and not line.startswith("#") and not line.startswith("-"):
                description = line[:200]
                break
        
        # Extract trigger keywords
        triggers = []
        
        # Look for "trigger" or "use when" sections
        trigger_match = re.search(
            r'(?:trigger|use when|keywords?)[:\s]+(.*?)(?:\n\n|\n#)',
            content, re.IGNORECASE | re.DOTALL,
        )
        if trigger_match:
            text = trigger_match.group(1)
            # Extract quoted strings or comma-separated words
            triggers = re.findall(r'"([^"]+)"', text)
            if not triggers:
                triggers = [w.strip().strip('"').strip("'") for w in text.split(",")]
        
        # Also use the skill name as a trigger
        triggers.append(name.replace("-", " "))
        triggers = [t.lower().strip() for t in triggers if t.strip()]
        
        return LoadedSkill(
            name=name, description=description,
            content=content, triggers=triggers, path=path,
        )
    
    def match(self, task_description: str, max_skills: int = 3) -> list[LoadedSkill]:
        """Find skills that match a task description."""
        task_lower = task_description.lower()
        scored = []
        
        for skill in self.skills.values():
            score = 0
            
            # Check trigger keywords
            for trigger in skill.triggers:
                if trigger in task_lower:
                    score += 3
            
            # Check skill name in task
            if skill.name.replace("-", " ") in task_lower:
                score += 5
            
            # Check task keywords in skill content
            task_words = set(task_lower.split())
            content_lower = skill.content.lower()
            for word in task_words:
                if len(word) > 3 and word in content_lower:
                    score += 1
            
            if score > 0:
                scored.append((score, skill))
        
        scored.sort(key=lambda x: x[0], reverse=True)
        return [s[1] for s in scored[:max_skills]]
    
    def inject_into_prompt(self, task_description: str, max_chars: int = 4000) -> str:
        """Get skill content to inject into an agent's system prompt."""
        matched = self.match(task_description, max_skills=2)
        if not matched:
            return ""
        
        lines = ["## Relevant skill instructions:\n"]
        total_chars = 0
        
        for skill in matched:
            # Truncate skill content to fit
            remaining = max_chars - total_chars
            if remaining < 200:
                break
            
            content = skill.content[:remaining]
            lines.append(f"### {skill.name}")
            lines.append(content)
            lines.append("")
            total_chars += len(content)
        
        return "\n".join(lines)
    
    def list_skills(self) -> list[dict]:
        """List all loaded skills."""
        return [
            {"name": s.name, "description": s.description[:80], "triggers": s.triggers[:5]}
            for s in sorted(self.skills.values(), key=lambda s: s.name)
        ]
    
    def stats(self) -> dict:
        return {
            "total_skills": len(self.skills),
            "with_triggers": sum(1 for s in self.skills.values() if len(s.triggers) > 1),
        }
