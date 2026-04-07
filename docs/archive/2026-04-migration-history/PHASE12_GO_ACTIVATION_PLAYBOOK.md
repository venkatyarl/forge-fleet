# Phase 12 — GO Activation Playbook (HOLD/NO-GO → GO)

Date: 2026-04-04  
Repo: `/Users/venkat/taylorProjects/forge-fleet`  
Scope: operator run-sequence to flip release state from **HOLD/NO-GO** to **GO** once gates are satisfied.

---

## 0) Entry Criteria (required before flip)

- Current state is **HOLD** or **NO-GO**.
- All blocking items from `docs/PHASE11_FINAL_AUDIT.md` / `docs/PHASE11_GO_GATES.md` are resolved.
- Evidence artifacts are present and current (`docs/PHASE12_EVIDENCE_MATRIX.md`, release logs).

If any item above is false, remain **HOLD/NO-GO**.

---

## 1) Verification Step (operator execution)

Run from repo root and attach outputs to release thread/evidence bundle:

```bash
cargo check --workspace
cargo test --workspace --lib
cargo run -p ff-cli -- --help
git status --short
```

Verification pass criteria:
- All commands exit `0`.
- No unexpected working tree drift.
- Gate statuses can be marked PASS in Phase 11 gate sheet.

Record: timestamp, executor, command output links.

---

## 2) Decision Checkpoint (formal GO vote)

At scheduled go/no-go checkpoint (RC-led):

- Required approvers: **Engineering + Product + Ops**.
- Decision rule: **all gates PASS + no unresolved HIGH risk = GO**.
- Any FAIL/unknown risk keeps state at **NO-GO/HOLD**.

Decision log (mandatory):
- Candidate SHA
- Target tag
- Final decision (`GO` or `NO-GO`)
- Approver names/time

---

## 3) Communication Step (state flip announcement)

Immediately after GO decision, RC posts in release channel:

**Template**

```text
GO CONFIRMED — Phase 12 release activation
Tag: <RC_TAG>
SHA: <RC_SHA>
Decision time: <timestamp>
Approvers: Eng=<name>, Product=<name>, Ops=<name>
Next action: proceed to tag cut sequence (RE owner)
Rollback authority: IC active until stabilization gate
```

Then update status artifacts:
- `docs/PHASE12_STATUS_DASHBOARD.md` (state: GO)
- release thread pinned message (GO + SHA/tag)

---

## 4) Tag-Cut Readiness Checklist (must be all checked)

Before executing `git tag` / `git push`:

- [ ] `RC_SHA` matches approved checkpoint SHA.
- [ ] `RC_TAG` matches release naming convention and is unused remotely.
- [ ] `git status --short` is clean (or only explicitly approved release artifacts).
- [ ] `git show --no-patch --decorate "$RC_SHA"` reviewed in channel.
- [ ] `git push --dry-run origin "$RC_TAG"` succeeds.
- [ ] Checksum/artifact plan prepared (per `docs/PHASE12_RELEASE_DAY_CHECKLIST.md`).
- [ ] Rollback command path confirmed (`docs/PHASE12_RELEASE_COMMANDS.md`).

If any item is unchecked, stop and remain GO-authorized-but-not-cut.

---

## 5) GO Flip Execution Sequence (exact order)

1. Complete verification step and mark gates PASS.  
2. Run formal decision checkpoint and capture approvals.  
3. Publish GO communication in release channel.  
4. Execute tag-cut readiness checklist.  
5. Authorize RE to cut/push tag per `docs/PHASE12_RELEASE_DAY_CHECKLIST.md`.  
6. Post “tag cut complete” confirmation (tag + SHA + checksum).  

This is the only approved sequence for HOLD/NO-GO → GO activation.

---

## References

- `docs/PHASE11_GO_GATES.md`
- `docs/PHASE11_FINAL_AUDIT.md`
- `docs/PHASE12_EVIDENCE_MATRIX.md`
- `docs/PHASE12_RELEASE_DAY_CHECKLIST.md`
- `docs/PHASE12_RELEASE_COMMANDS.md`
