# Phase 11 — GO/NO-GO Gate Checklist (ForgeFleet Rust Rewrite)

Date: 2026-04-04  
Repo: `/Users/venkat/taylorProjects/forge-fleet`  
Applies to target tag: `v0.1.0-internal`

This checklist is the strict go-live gate for internal release. It is derived from:
- `docs/PHASE10_RELEASE_READINESS.md` (minimum cut criteria)
- `docs/PHASE10_SHIP_PLAN.md` (verification + rollback plan)
- `docs/PHASE10_OPERATOR_RUNBOOK.md` (ops startup/health verification)
- `docs/PHASE11_FINAL_AUDIT.md` (NO-GO rationale + flip-to-GO criteria)
- `docs/PHASE11_RELEASE_CANDIDATE_NOTES.md` (blockers + deferred scope)
- `docs/PHASE11_HANDOFF_PACK.md` (daily/mini release gates)

---

## Decision rule (strict)

- **GO** only if **every gate below is PASS**.
- **Any FAIL = NO-GO**.
- Sign-off must be completed by **Engineering, Product, and Ops**.

---

## Gate status summary

| Gate ID | Gate | Status | Evidence Link / Notes |
|---|---|---|---|
| G1 | Workspace compile gate | ☐ PASS ☐ FAIL |  |
| G2 | Workspace test gate | ☐ PASS ☐ FAIL |  |
| G3 | CLI contract smoke gate | ☐ PASS ☐ FAIL |  |
| G4 | Release content integrity gate (no critical untracked drift) | ☐ PASS ☐ FAIL |  |
| G5 | CI enforcement gate (check + test on push/PR) | ☐ PASS ☐ FAIL |  |
| G6 | Integration maturity / explicit deferment gate | ☐ PASS ☐ FAIL |  |
| G7 | Fresh smoke artifact gate | ☐ PASS ☐ FAIL |  |
| G8 | Ops startup + health gate | ☐ PASS ☐ FAIL |  |
| G9 | Rollback readiness gate | ☐ PASS ☐ FAIL |  |
| G10 | Release notes / governance disclosure gate | ☐ PASS ☐ FAIL |  |

---

## G1 — Workspace compile gate

**Verification commands (run from repo root):**

```bash
cargo check --workspace
```

**PASS criteria:** command exits `0`.

**Recorded result:** ☐ PASS ☐ FAIL  
**Evidence:** __________________________________________

---

## G2 — Workspace test gate

**Verification commands:**

```bash
cargo test --workspace --lib
```

**PASS criteria:** command exits `0` and no crate reports failing tests.

**Recorded result:** ☐ PASS ☐ FAIL  
**Evidence:** __________________________________________

---

## G3 — CLI contract smoke gate

**Verification commands:**

```bash
cargo run -p ff-cli -- --help
cargo run -p ff-cli -- status
cargo run -p ff-cli -- health
```

**PASS criteria:** all commands exit `0`; help output and status/health commands respond successfully.

**Recorded result:** ☐ PASS ☐ FAIL  
**Evidence:** __________________________________________

---

## G4 — Release content integrity gate (no critical untracked drift)

**Verification commands:**

```bash
git status --short
git status --short | grep -E '^\?\? (crates|src|docs)/' || true
```

**PASS criteria:**
- `git status --short` has no unexpected release-content drift, and
- no critical intended release assets remain untracked.

> Phase 10/11 explicitly call untracked release surface a high-risk blocker.

**Recorded result:** ☐ PASS ☐ FAIL  
**Evidence:** __________________________________________

---

## G5 — CI enforcement gate (check + test on push/PR)

**Verification commands:**

```bash
find .github/workflows -maxdepth 1 -type f \( -name '*.yml' -o -name '*.yaml' \) -print
grep -RIn "cargo check --workspace" .github/workflows
grep -RIn "cargo test --workspace --lib" .github/workflows
```

**PASS criteria:**
- At least one workflow file exists, and
- workflow definitions include both workspace check and workspace lib-test gates.

**Recorded result:** ☐ PASS ☐ FAIL  
**Evidence:** __________________________________________

---

## G6 — Integration maturity / explicit deferment gate

**Verification commands:**

```bash
grep -n "Hello, world!" src/main.rs || true
grep -nEi "placeholder|todo!|unimplemented!" crates/ff-pipeline/src/lib.rs || true
grep -nE "ff-pipeline|src/main.rs|Deferred scope|Release blockers" docs/PHASE11_FINAL_AUDIT.md docs/PHASE11_RELEASE_CANDIDATE_NOTES.md
```

**PASS criteria (strict):**
- Either integration placeholders are closed in code, **or**
- placeholders are explicitly documented as deferred/release-limited with sign-off and risk acceptance.

**Recorded result:** ☐ PASS ☐ FAIL  
**Evidence:** __________________________________________

---

## G7 — Fresh smoke artifact gate

**Verification commands:**

```bash
mkdir -p .phase9-smoke
cargo check --workspace > .phase9-smoke/cargo_check.log 2>&1
cargo test --workspace --lib > .phase9-smoke/cargo_test_workspace_lib.log 2>&1
cargo run -p ff-cli -- --help > .phase9-smoke/ff_cli_help.log 2>&1
ls -lh .phase9-smoke/cargo_check.log .phase9-smoke/cargo_test_workspace_lib.log .phase9-smoke/ff_cli_help.log
```

**PASS criteria:** all three commands exit `0` and fresh log artifacts are generated.

**Recorded result:** ☐ PASS ☐ FAIL  
**Evidence:** __________________________________________

---

## G8 — Ops startup + health gate

> Use terminals/sessions as needed (per Operator Runbook).

**Verification commands:**

```bash
# Terminal A (API)
RUST_LOG=info cargo run -p ff-api

# Terminal B (health checks after API is up)
curl -fsS http://127.0.0.1:4000/health
cargo run -p ff-cli -- status
cargo run -p ff-cli -- health

# Terminal C (agent)
FF_AGENT_NODE_ID=node-1 \
FF_LEADER_URL=http://127.0.0.1:51819 \
FF_RUNTIME_URL=http://127.0.0.1:8000 \
FF_AGENT_HTTP_PORT=51820 \
cargo run -p ff-agent

# Terminal B (agent verification)
curl -fsS http://127.0.0.1:51820/health
curl -fsS http://127.0.0.1:51820/status
cargo run -p ff-cli -- nodes
```

**PASS criteria:** API and agent health endpoints respond; CLI status/health/nodes checks succeed.

**Recorded result:** ☐ PASS ☐ FAIL  
**Evidence:** __________________________________________

---

## G9 — Rollback readiness gate

**Verification commands:**

```bash
git log --oneline --decorate -n 20
grep -nE "git revert -m 1 <merge_commit_sha>|git tag -d v0.1.0-internal|git push --delete origin v0.1.0-internal" docs/PHASE10_SHIP_PLAN.md docs/PHASE10_OPERATOR_RUNBOOK.md
```

**Optional dry-run safety check (local only):**

```bash
git revert -m 1 --no-commit <merge_commit_sha>
git revert --abort
```

**PASS criteria:** rollback commands are documented and understood by on-call owners; optional dry-run succeeds.

**Recorded result:** ☐ PASS ☐ FAIL  
**Evidence:** __________________________________________

---

## G10 — Release notes / governance disclosure gate

**Verification commands:**

```bash
grep -nE "HOLD / NO-GO|Release blockers|Deferred scope|ff-pipeline|ff-deploy" docs/PHASE11_RELEASE_CANDIDATE_NOTES.md
grep -nE "Recommended cut criteria|Tag v0.1.0-internal|In progress|NO-GO" docs/PHASE10_RELEASE_READINESS.md docs/PHASE11_FINAL_AUDIT.md
```

**PASS criteria:** release notes/readiness docs explicitly disclose blockers, deferred scope, and gate state.

**Recorded result:** ☐ PASS ☐ FAIL  
**Evidence:** __________________________________________

---

## Final gate computation

- [ ] All gates G1–G10 are marked **PASS**.
- [ ] No unresolved HIGH risk remains without explicit acceptance.
- [ ] Sign-off block below is fully completed.

**Final decision:** ☐ GO  ☐ NO-GO

---

## Sign-off block (required)

### Engineering Sign-off
- Name: ______________________________
- Role: _______________________________
- Decision: ☐ GO ☐ NO-GO
- Notes / risk exceptions: __________________________________________
- Signature: __________________________
- Date/Time: __________________________

### Product Sign-off
- Name: ______________________________
- Role: _______________________________
- Decision: ☐ GO ☐ NO-GO
- Notes / scope acceptance: __________________________________________
- Signature: __________________________
- Date/Time: __________________________

### Ops Sign-off
- Name: ______________________________
- Role: _______________________________
- Decision: ☐ GO ☐ NO-GO
- Notes / rollback readiness: __________________________________________
- Signature: __________________________
- Date/Time: __________________________

---

## References

- `docs/PHASE10_RELEASE_READINESS.md`
- `docs/PHASE10_SHIP_PLAN.md`
- `docs/PHASE10_OPERATOR_RUNBOOK.md`
- `docs/PHASE11_FINAL_AUDIT.md`
- `docs/PHASE11_RELEASE_CANDIDATE_NOTES.md`
- `docs/PHASE11_HANDOFF_PACK.md`
