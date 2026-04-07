# Phase 9 Smoke Checklist

## Executable smoke checklist

1. From repo root, ensure toolchain is available:
   - `cargo --version`
2. Run workspace compile check and capture log:
   - `cargo check --workspace > .phase9-smoke/cargo_check.log 2>&1`
3. Run library test suite and capture log:
   - `cargo test --workspace --lib > .phase9-smoke/cargo_test_workspace_lib.log 2>&1`
4. Run CLI help smoke and capture log:
   - `cargo run -p ff-cli -- --help > .phase9-smoke/ff_cli_help.log 2>&1`
5. Verify expected success markers in logs:
   - `Finished \`dev\` profile` for check/help
   - `test result: ok.` and no failures for tests
6. Record final pass/fail matrix (below).

## Smoke run notes (2026-04-04)

- Initial `cargo check --workspace` failed with `E0583` (missing `ff-control` modules declared in `lib.rs`).
- Applied minimal compile fix by adding:
  - `crates/ff-control/src/commands.rs`
  - `crates/ff-control/src/control_plane.rs`
  - `crates/ff-control/src/health.rs`
- Retried failed check once; retry passed.
- `cargo test --workspace --lib` passed (`279 passed, 0 failed`, aggregated from crate-level test summaries).
- `cargo run -p ff-cli -- --help` passed and printed ForgeFleet CLI usage.

## Pass/Fail matrix

| Command | First run | Retry (if needed) | Final | Evidence |
|---|---|---|---|---|
| `cargo check --workspace` | FAIL (`E0583` missing module files in `ff-control`) | PASS | PASS | `.phase9-smoke/cargo_check.log` (contains "Finished dev profile") |
| `cargo test --workspace --lib` | PASS | N/A | PASS | `.phase9-smoke/cargo_test_workspace_lib.log` (`aggregate: 279 passed / 0 failed`) |
| `cargo run -p ff-cli -- --help` | PASS | N/A | PASS | `.phase9-smoke/ff_cli_help.log` (prints `ForgeFleet unified AI operating system`) |
