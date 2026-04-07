# Phase 12 — Release Communication Pack (ForgeFleet Rust Rewrite)

Date: `<YYYY-MM-DD>`  
Release Version/Tag: `<vX.Y.Z or vX.Y.Z-rc.N>`  
Owner: `<name>`  
Channel(s): `<engineering channel / leadership channel / incident channel>`

> Purpose: ready-to-send communication templates for Phase 12 release execution, leadership visibility, and incident/rollback handling.

---

## Key Docs (Link Bundle)

Use these links in messages to keep all updates anchored to the same source of truth:

- [Docs Index](./INDEX.md)
- [Phase 12 Release Day Checklist](./PHASE12_RELEASE_DAY_CHECKLIST.md)
- [Phase 12 Release Commands](./PHASE12_RELEASE_COMMANDS.md)
- [Phase 12 Release Execution Template](./PHASE12_RELEASE_EXECUTION_TEMPLATE.md)
- [Phase 12 Post-Release Monitoring](./PHASE12_POST_RELEASE_MONITORING.md)
- [Phase 12 Sign-off Package](./PHASE12_SIGNOFF_PACKAGE.md)
- [Phase 12 Status Dashboard](./PHASE12_STATUS_DASHBOARD.md)
- [Phase 12 Decision Memo](./PHASE12_DECISION_MEMO.md)
- [Phase 11 GO Gates](./PHASE11_GO_GATES.md)

---

## Template 1 — Engineering Update (Ready to Send)

```text
Subject: [ForgeFleet][Phase 12] Engineering Release Update — <vX.Y.Z> — <GO/HOLD>

Date: <YYYY-MM-DD HH:MM TZ>
Owner: <name>
Release: <vX.Y.Z>
Current Status: <GO / HOLD / NO-GO / IN PROGRESS>

Team — quick Phase 12 engineering update:

1) Release Window
- Window: <start time> → <end time>
- Current checkpoint: <T-30 / T+15 / T+60 / etc>
- Coordinators: <RC>, <RE>, <QE>, <OE>

2) Gate Status
- Gate A (Preflight): <PASS/FAIL/BLOCKED>
- Gate B (Build Artifacts): <PASS/FAIL/BLOCKED>
- Gate C (Tests + Smoke): <PASS/FAIL/BLOCKED>
- Gate D (Tag + Release Notes): <PASS/FAIL/BLOCKED>
- Gate E (Post-release Validation): <PASS/FAIL/BLOCKED>

3) Command + Evidence Snapshot
- Candidate SHA: <git_sha>
- Candidate Tag: <vX.Y.Z-rc.N>
- Checksum: <sha256>
- Evidence folder/logs: <path or link>

4) Current Risks / Blockers
- <risk 1>
- <risk 2>
- <none>

5) Next Action + ETA
- Next action: <what happens next>
- ETA to next update: <N minutes>

Reference docs:
- Release Day Checklist: ./docs/PHASE12_RELEASE_DAY_CHECKLIST.md
- Release Commands: ./docs/PHASE12_RELEASE_COMMANDS.md
- Monitoring Plan: ./docs/PHASE12_POST_RELEASE_MONITORING.md
```

---

## Template 2 — Leadership Summary (Ready to Send)

```text
Subject: [ForgeFleet][Phase 12] Leadership Release Summary — <vX.Y.Z> — <GO/HOLD>

Date: <YYYY-MM-DD HH:MM TZ>
Owner: <name>
Release: <vX.Y.Z>
Decision Status: <GO / HOLD / NO-GO>

Executive summary:
- Overall status: <GREEN / YELLOW / RED>
- Decision: <GO/HOLD/NO-GO> for <vX.Y.Z>
- Why: <one-line rationale>

Business/operational impact:
- User impact: <none / low / moderate / high>
- Platform risk: <low / medium / high>
- Expected stabilization window: <T+2h / T+24h>

Top 3 facts:
1) <fact 1>
2) <fact 2>
3) <fact 3>

Open risks / asks:
- <risk or decision needed>
- <owner + deadline>

Next checkpoint:
- Time: <YYYY-MM-DD HH:MM TZ>
- Deliverable: <go/no-go confirmation, stabilization update, or rollback decision>

Reference docs:
- Status Dashboard: ./docs/PHASE12_STATUS_DASHBOARD.md
- Decision Memo: ./docs/PHASE12_DECISION_MEMO.md
- Sign-off Package: ./docs/PHASE12_SIGNOFF_PACKAGE.md
```

---

## Template 3 — Incident / Rollback Notice (Ready to Send)

```text
Subject: [ForgeFleet][INCIDENT] Phase 12 Rollback Notice — <vX.Y.Z> → <rollback_tag>

Date: <YYYY-MM-DD HH:MM TZ>
Owner (Incident Commander): <name>
Release: <vX.Y.Z>
Incident Severity: <SEV-1 / SEV-2 / SEV-3>
Status: <ROLLBACK INITIATED / ROLLBACK COMPLETE / MONITORING>

What happened:
- At <time>, we observed <symptom/trigger> during/after release <vX.Y.Z>.
- Trigger met: <Decision Point A/B/C or explicit rollback trigger>

Impact:
- Affected scope: <service/command/user segment>
- Current impact level: <low / moderate / high>

Action taken:
- Rollback approved by: <name>
- Rollback target: <rollback_tag> (<rollback_sha>)
- Commands/runbook used: ./docs/PHASE12_RELEASE_COMMANDS.md (Rollback section)

Current state:
- Health now: <green/yellow/red>
- Validation checks: <check/test/smoke status>
- Remaining risk: <none or short list>

Next update:
- ETA: <YYYY-MM-DD HH:MM TZ>
- Owner: <name>

Reference docs:
- Release Commands: ./docs/PHASE12_RELEASE_COMMANDS.md
- Post-Release Monitoring: ./docs/PHASE12_POST_RELEASE_MONITORING.md
- Release Day Checklist: ./docs/PHASE12_RELEASE_DAY_CHECKLIST.md
```

---

## Usage Notes

- Keep one active owner per message (`Owner: <name>`).
- Always include date/time, version/tag, and current decision state.
- Reuse the same candidate SHA/tag across all communications to prevent drift.
- For any rollback communication, include trigger + approval + target SHA/tag.
