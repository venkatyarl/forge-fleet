# Fleet-wide Inventory: Stray `hive` / `hive-mind` Directories

- **Date:** 2026-07-20
- **Type:** Read-only audit (no deletions, no modifications)
- **Scope:** All online fleet computers and every `~/.forgefleet/sub-agents/sub-agent-*` slot
- **Goal:** Identify directories named `hive` or `hive-mind` that live **outside** forge-fleet repos, report their paths, and note whether each is empty.

## Method

On each host the following read-only sweep was run against `$HOME`:

```sh
find "$HOME" -type d \( -name hive -o -name hive-mind \) -prune
```

Each match was then classified:
- **empty?** — `EMPTY` if `ls -A` returns nothing, else `NONEMPTY`.
- **location?** — `IN-REPO` if the path contains `forge-fleet`, else `STRAY` (outside a forge-fleet repo).

The local box (`adele`) additionally received a broader case-insensitive `*hive*`
sweep to catch odd casings; every hit was the substring inside "arch**ive**",
not an actual `hive` directory.

## Roster covered

Source of truth: `computers` table (live Postgres), 18 rows.

| Host | Status | Result |
|------|--------|--------|
| adele (local) | online | clean — 0 hive/hive-mind dirs |
| ace | online | clean |
| aura | online | clean |
| beyonce | online | clean |
| duncan | online | clean |
| james | online | clean |
| lily | online | clean |
| logan | online | clean |
| marcus | online | clean |
| priya | online | clean |
| rihanna | online | clean |
| sarah | online | clean |
| shakira | online | clean |
| sia | online | clean |
| sophie | online | clean |
| thalia | online | clean |
| veronica | online | clean |
| taylor | **offline** since 2026-04-24 | NOT AUDITED (unreachable) |

## Findings

**No stray `hive` or `hive-mind` directories were found anywhere on the fleet.**

- 17 online computers audited (including the local `adele` box and all
  `~/.forgefleet/sub-agents/sub-agent-*` slots on each). Every host returned zero
  directories named `hive` or `hive-mind`, whether inside or outside forge-fleet repos.
- 1 computer (`taylor`) is offline (last seen 2026-04-24) and could not be reached;
  it should be re-audited when it comes back online.

## Notes

- `hive-mind` exists as a **concept/branch** in the codebase
  (e.g. `feature/audit-hive-mind-integration-gaps-*`), but there is no corresponding
  stray **filesystem directory** by that name on any audited host.
- No files or directories were created, moved, or deleted during this audit
  (this report is the only artifact produced).
