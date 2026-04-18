# Phase 12 — Final Sign-off Package (ForgeFleet Rust Rewrite)

Date: `<YYYY-MM-DD>`  
Release candidate tag: `<vX.Y.Z-rc.N>`  
Release SHA: `<git_sha>`  
Repo: `/Users/venkat/projects/forge-fleet`

> Purpose: single sign-off package for final release decisioning.  
> Rule: **GO requires all mandatory checklist items complete, evidence attached, and all four sign-offs (Engineering, Product, Ops, QA).**

---

## 1) Final Artifact Checklist

### A) Code Artifacts (must be present and validated)

| Item | Status | Evidence path / link | Notes |
|---|---|---|---|
| Workspace manifests are present (`Cargo.toml`, `Cargo.lock`) | ☐ Done ☐ N/A | `Cargo.toml`, `Cargo.lock` | Baseline dependency and workspace integrity |
| Release candidate commit is clean and reproducible | ☐ Done ☐ N/A | `git status --short`, `git rev-parse --short HEAD`, `.phase12-release/release_sha.txt` | Must match approved SHA |
| Release binary generated from approved SHA | ☐ Done ☐ N/A | `target/release/forgefleet`, `.phase12-release/cargo_build_release.log` | Build from same candidate commit |
| Release checksum generated | ☐ Done ☐ N/A | `.phase12-release/forgefleet.sha256` | Used for provenance verification |
| Smoke/validation commands pass on candidate | ☐ Done ☐ N/A | `.phase12-release/cargo_check.log`, `.phase12-release/cargo_test_workspace_lib.log`, `.phase9-smoke/ff_cli_help.log` | Command evidence must be fresh |

### B) Documentation Artifacts (must be complete and current)

| Item | Status | Evidence path / link | Notes |
|---|---|---|---|
| Final audit baseline reviewed | ☐ Done ☐ N/A | [docs/PHASE11_FINAL_AUDIT.md](./PHASE11_FINAL_AUDIT.md) | Contains prior NO-GO rationale and closure criteria |
| GO/NO-GO gates completed with evidence | ☐ Done ☐ N/A | [docs/PHASE11_GO_GATES.md](./PHASE11_GO_GATES.md) | G1–G10 all PASS required |
| Remediation closure validated | ☐ Done ☐ N/A | [docs/PHASE11_REMEDIATION_PLAN.md](./PHASE11_REMEDIATION_PLAN.md) | All GO-critical items resolved or formally deferred |
| Release execution plan filled in | ☐ Done ☐ N/A | [docs/PHASE12_RELEASE_EXECUTION_TEMPLATE.md](./PHASE12_RELEASE_EXECUTION_TEMPLATE.md) | Time-phased execution record |
| Release day operator checklist completed | ☐ Done ☐ N/A | [docs/PHASE12_RELEASE_DAY_CHECKLIST.md](./PHASE12_RELEASE_DAY_CHECKLIST.md) | Includes rollback triggers and evidence expectations |
| Release/rollback commands validated | ☐ Done ☐ N/A | [docs/PHASE12_RELEASE_COMMANDS.md](./PHASE12_RELEASE_COMMANDS.md) | Operator command source of truth |
| Post-release monitoring plan attached | ☐ Done ☐ N/A | [docs/PHASE12_POST_RELEASE_MONITORING.md](./PHASE12_POST_RELEASE_MONITORING.md) | T+0 to T+24h controls |
| Consolidation/executive map reviewed | ☐ Done ☐ N/A | [docs/PHASE12_FINAL_CONSOLIDATION.md](./PHASE12_FINAL_CONSOLIDATION.md) | Canonical phase map and decision context |

### C) CI / Governance Artifacts (must be enforced)

| Item | Status | Evidence path / link | Notes |
|---|---|---|---|
| CI workflow exists and is versioned | ☐ Done ☐ N/A | [`.github/workflows/rust-quality-gates.yml`](../.github/workflows/rust-quality-gates.yml) | Required pipeline definition |
| Required CI gates configured (fmt/clippy/check/test) | ☐ Done ☐ N/A | [docs/PHASE12_CI_BOOTSTRAP.md](./PHASE12_CI_BOOTSTRAP.md), [docs/PHASE12_QUALITY_GATE_AUTOMATION.md](./PHASE12_QUALITY_GATE_AUTOMATION.md) | Must match branch protection checks |
| Branch protection requires quality checks on `main` | ☐ Done ☐ N/A | GitHub branch protection screenshot / URL | Capture date/time of verification |
| Toolchain baseline parity confirmed | ☐ Done ☐ N/A | [docs/PHASE12_TOOLCHAIN_BASELINE.md](./PHASE12_TOOLCHAIN_BASELINE.md) | Rust 1.85.0 parity local vs CI |

---

## 2) Required Evidence Links / Paths (Minimum Set)

Attach or reference the following before final sign-off:

### Repository documents
- `docs/PHASE11_FINAL_AUDIT.md`
- `docs/PHASE11_GO_GATES.md`
- `docs/PHASE11_REMEDIATION_PLAN.md`
- `docs/PHASE12_CI_BOOTSTRAP.md`
- `docs/PHASE12_QUALITY_GATE_AUTOMATION.md`
- `docs/PHASE12_RELEASE_EXECUTION_TEMPLATE.md`
- `docs/PHASE12_RELEASE_DAY_CHECKLIST.md`
- `docs/PHASE12_RELEASE_COMMANDS.md`
- `docs/PHASE12_POST_RELEASE_MONITORING.md`
- `docs/PHASE12_FINAL_CONSOLIDATION.md`

### CI / workflow evidence
- `.github/workflows/rust-quality-gates.yml`
- CI run URL(s) for candidate SHA showing green status for:
  - `cargo fmt --check`
  - `cargo clippy --workspace -- -D warnings`
  - `cargo check --workspace`
  - `cargo test --workspace --lib`
- Branch protection rule capture for `main` (URL and/or screenshot)

### Command/log artifact evidence
- `.phase12-release/release_sha.txt`
- `.phase12-release/cargo_fmt_check.log`
- `.phase12-release/cargo_clippy.log`
- `.phase12-release/cargo_check.log`
- `.phase12-release/cargo_test_workspace_lib.log`
- `.phase12-release/cargo_build_release.log`
- `.phase12-release/forgefleet.sha256`
- `.phase9-smoke/cargo_check.log`
- `.phase9-smoke/cargo_test_workspace_lib.log`
- `.phase9-smoke/ff_cli_help.log`
- (if rollback triggered)
  - `.phase12-release/rollback_target_sha.txt`
  - `.phase12-release/rollback_diff_files.txt`
  - `.phase12-release/rollback_cargo_check.log`
  - `.phase12-release/rollback_cargo_test_lib.log`

---

## 3) Final Sign-off Table (Required)

| Function | Owner | Decision (GO / NO-GO) | Date/Time (UTC) | Signature / Ack | Evidence reviewed |
|---|---|---|---|---|---|
| Engineering | `<name>` | `<GO or NO-GO>` | `<YYYY-MM-DD HH:MM>` | `<initials/signature>` | `<links/paths>` |
| Product | `<name>` | `<GO or NO-GO>` | `<YYYY-MM-DD HH:MM>` | `<initials/signature>` | `<links/paths>` |
| Ops | `<name>` | `<GO or NO-GO>` | `<YYYY-MM-DD HH:MM>` | `<initials/signature>` | `<links/paths>` |
| QA | `<name>` | `<GO or NO-GO>` | `<YYYY-MM-DD HH:MM>` | `<initials/signature>` | `<links/paths>` |

Decision rule:
- **GO** only if all four rows are GO and all mandatory artifacts/evidence are complete.
- Any NO-GO, missing evidence, or unresolved P0/P1 risk => **NO-GO / HOLD**.

---

## 4) Explicit GO Decision Statement Template

Copy/paste template:

```text
GO DECISION — FORGEFLEET RUST REWRITE

As of <YYYY-MM-DD HH:MM TZ>, release candidate <TAG> at commit <SHA> is approved for GO.

We confirm:
1) All mandatory release gates are PASS (G1–G10 in docs/PHASE11_GO_GATES.md).
2) Required CI checks are green for the candidate SHA.
3) Required evidence artifacts are present and reviewed.
4) Final sign-offs are complete from Engineering, Product, Ops, and QA.
5) Rollback plan and commands are validated and on-call ownership is assigned.

Authorized release action: Proceed with tag/release execution per docs/PHASE12_RELEASE_COMMANDS.md.

Approved by:
- Engineering: <name>
- Product: <name>
- Ops: <name>
- QA: <name>
```

Optional NO-GO fallback statement:

```text
NO-GO / HOLD DECISION

As of <YYYY-MM-DD HH:MM TZ>, candidate <TAG>@<SHA> remains NO-GO due to: <blocking reasons>.
Release is held until: <required closure actions + evidence>.
```
