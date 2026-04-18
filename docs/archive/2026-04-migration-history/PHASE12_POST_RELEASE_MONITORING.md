# Phase 12 — Post-Release Monitoring Plan (First 24h)

Date: 2026-04-04  
Repo: `/Users/venkat/projects/forge-fleet`  
Applies to release/tag: `<vX.Y.Z>`

> Purpose: define the first 24 hours of post-release monitoring for the ForgeFleet Rust rewrite, including key signals, thresholds, escalation flow, and rollback decision points.

---

## 1) Monitoring objectives

1. Detect regressions quickly after release cut.
2. Protect operator workflows (`ff-cli` commands, API health, agent availability).
3. Escalate consistently by severity (not ad-hoc judgment).
4. Trigger rollback only at defined decision points with clear evidence.

---

## 2) Coverage window and cadence (T+0h to T+24h)

- **T+0h to T+2h:** high-frequency checks every 15 minutes.
- **T+2h to T+8h:** checks every 30 minutes.
- **T+8h to T+24h:** checks hourly.
- Record every check in the hourly status log (template in Section 7).

---

## 3) Key signals and thresholds

Capture a **baseline at T+0h** (first successful post-release snapshot). Use both absolute thresholds and degradation vs baseline.

| Signal | What to Watch | Green | Warning (SEV-3) | Critical (SEV-2/SEV-1) |
|---|---|---|---|---|
| **Build health** | Required release CI workflows (`check`, `test`, release smoke) | All required workflows passing | 1 workflow failure with known fix in progress (<30 min) | 2 consecutive failures on same workflow, or any required workflow red for >45 min |
| **Command latency** | `ff-cli status`, `ff-cli health`, `ff-cli nodes` (p95) | p95 within baseline +25% and <2.0s | p95 baseline +25–75% or 2.0–4.0s for >15 min | p95 > baseline +75% or >4.0s for >10 min |
| **Errors** | API 5xx rate, CLI non-zero exits, panic/crash logs | 5xx <0.5%, command failures <1% | 5xx 0.5–2% or command failures 1–3% for >10 min | 5xx >2% for >10 min, command failures >3% for >10 min, or any repeated panic/crash loop |
| **Agent heartbeat** | Agent liveness / last-seen timestamps | 100% nodes heartbeat within 60s expected interval | Any node stale >2 min | Any node stale >5 min, or >20% nodes stale >2 min for >10 min |

### Minimum signal sources

- CI system (required workflow status for release branch/tag).
- CLI checks from operator terminal:
  - `cargo run -p ff-cli -- status`
  - `cargo run -p ff-cli -- health`
  - `cargo run -p ff-cli -- nodes`
- Service health endpoints (`/health`, `/status`) for API and agent.
- Runtime logs for panic/error spikes.

---

## 4) Severity levels

- **SEV-3 (Warning):** early degradation, no hard outage yet.
- **SEV-2 (Major):** sustained degradation or partial outage affecting operators/workflows.
- **SEV-1 (Critical):** high-impact outage, repeated crashes, or rollback trigger met.

---

## 5) Escalation matrix

| Severity | Trigger Example | Who Responds | Acknowledge SLA | Action Window |
|---|---|---|---|---|
| **SEV-3** | Single CI failure, moderate latency increase, single stale node | On-duty operator + release coordinator | 15 min | Mitigate within 60 min, monitor closely |
| **SEV-2** | Sustained latency/error breach, repeated command failures, multi-node heartbeat issues | Operator + release coordinator + engineering lead | 10 min | Contain within 30 min; prep rollback evidence |
| **SEV-1** | Severe error-rate breach, crash loop, critical command path unavailable, rollback gate hit | Incident commander + eng lead + ops lead + product owner notified | 5 min | Decide rollback within 15 min |

### Escalation flow

1. Operator detects threshold breach and declares provisional severity.
2. Release coordinator validates signal/evidence and confirms severity.
3. If SEV-2/SEV-1: open incident bridge/channel immediately.
4. If rollback decision point is met: incident commander calls rollback (Section 6).

---

## 6) Rollback decision points

Rollback target must be the last known-good tag validated pre-release.

### Decision Point A — Immediate protection (T+0h to T+2h)

Rollback if **any** of the following occurs:
- SEV-1 condition lasts >15 minutes.
- Critical command path (`status`, `health`, or `nodes`) unavailable for >10 minutes.
- Repeated panic/crash loop with no stable period for 15 minutes.

### Decision Point B — Sustained instability (T+2h to T+8h)

Rollback if **both** are true:
- Two or more SEV-2 incidents in a 4-hour window, **and**
- No confirmed fix with verified recovery after mitigation attempt.

### Decision Point C — 24h stability gate (T+8h to T+24h)

Hold release and consider rollback if:
- Error/latency remains outside green thresholds for >20% of hourly checks, or
- Agent heartbeat remains unstable (>20% nodes stale) across 2 consecutive hourly checks.

### Rollback execution prerequisites

- Incident commander approval recorded.
- Rollback tag/SHA identified and verified.
- Stakeholder notification sent (start + complete).
- Post-rollback smoke checks pass (`check`, `test`, CLI health/status/nodes).

---

## 7) Operator hourly status template

Use one entry per hour (or per high-frequency interval in the first 8 hours).

```markdown
## Post-Release Hourly Status — <YYYY-MM-DD HH:00 TZ>

- Release Tag: <vX.Y.Z>
- Operator: <name>
- Window: <T+Nh>

### 1) Signal Snapshot
- Build Health: <GREEN/WARN/CRITICAL> | Details: <workflow names + links>
- Command Latency (p95):
  - ff-cli status: <value>
  - ff-cli health: <value>
  - ff-cli nodes: <value>
  - Baseline delta: <+/- %>
- Errors:
  - API 5xx rate: <value>
  - CLI failure rate: <value>
  - Panic/crash events this window: <count>
- Agent Heartbeat:
  - Healthy nodes: <n/total>
  - Stale nodes (>2m): <count>
  - Stale nodes (>5m): <count>

### 2) Severity + Escalation
- Current Severity: <NONE/SEV-3/SEV-2/SEV-1>
- Escalated To: <none or names/roles>
- Ticket/Incident Link: <link>

### 3) Rollback Gate Check
- Decision Point Applicable: <A/B/C/N/A>
- Rollback Trigger Met? <Yes/No>
- If Yes: <who approved + timestamp + target tag>

### 4) Actions Taken
- <action 1>
- <action 2>

### 5) Next Check Focus
- <what to verify next>
```

---

## 8) 24h completion criteria

Release is considered stable at T+24h only if all are true:
- No unresolved SEV-1 incidents.
- No active SEV-2 incident at handoff.
- Build health restored to green.
- Command latency and error rates back within green thresholds.
- Agent heartbeat stable (no persistent stale-node pattern).

If criteria are not met, continue incident mode and defer release closeout.

---

## 9) References

- `docs/PHASE12_RELEASE_COMMANDS.md`
- `docs/PHASE12_RELEASE_EXECUTION_TEMPLATE.md`
- `docs/PHASE11_GO_GATES.md`
- `docs/PHASE10_OPERATOR_RUNBOOK.md`
