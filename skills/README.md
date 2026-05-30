# ForgeFleet skills

This directory holds **ff-native skills** that ship with the codebase. Every fleet member's auto-upgrade pipeline keeps a clone of forge-fleet at `~/.forgefleet/sub-agent-0/forge-fleet/`, so skills committed here propagate fleet-wide on the next `forgefleetd_git` wave — no separate distribution.

## Format

Each skill is one folder containing a `SKILL.md` with YAML frontmatter:

```markdown
---
name: my-skill
description: |
  One- or two-sentence description. The agent reads this in the catalog
  and decides whether the user's prompt warrants invoking the skill.
triggers:
  - "trigger phrase 1"
  - "trigger phrase 2"
---

# Body

Full instructions for the agent. Read this when the catalog match fires.
```

`triggers` are optional but help the agent's match step.

## Discovery

`ff_agent::skill_catalog` (V68) walks every `enabled` row in the V69 `skill_sources` Postgres table at session start. The default sources include this directory at priority 60 (V70). An operator can add more sources at runtime:

```sql
INSERT INTO skill_sources (id, label, path, priority)
VALUES ('my-team-skills', 'team-shared library', '/srv/skills', 80);
```

Higher priority wins on id collision.

## Adding a new skill

1. Create `skills/<my-skill>/SKILL.md` with frontmatter + body.
2. Commit + push. The `forgefleetd_git` auto-upgrade wave propagates it to every fleet member within ~1 hour.
3. Next `ff supervise` / `ff run` discovers it via the catalog and the agent self-routes when triggers match.

That's the entire workflow. No code change, no daemon restart, no operator intervention beyond the git push.

## Why this directory exists

Other skill systems (Claude Code, Codex, OpenClaw, Kimi) hardcode their scan paths and ship native skills bundled in the binary. ff puts both the scan paths (V69) and the skill content (this folder + open-design's bundled skills + Claude Code's user-global path + project-scoped paths) under operator control. The agent's behavior is data, not code.
