# Local Completion Pipeline

This document describes the local completion pipeline for ForgeFleet, which validates changes before they are committed.

## Overview

The local completion pipeline ensures code quality and prevents database-related test failures in CI. It must be run locally before committing changes.

## Pipeline Steps

1. **Code Formatting**
   ```bash
   cargo +1.88.0 fmt --check
   ```

2. **Static Analysis**
   ```bash
   cargo +1.88.0 check
   ```

3. **Targeted Testing**
   Run tests relevant to your changes. For database-dependent tests:
   - Verify `FORGEFLEET_POSTGRES_URL` or `FORGEFLEET_DATABASE_URL` is set
   - Tests must early-return if database URLs are unset (see [DB Tests Rule](#db-tests-rule))

## Critical Rules

### DB Tests Rule
Any test requiring Postgres **must** early-return when database URLs are unset:
```rust
#[test]
fn my_db_test() {
    if std::env::var("FORGEFLEET_POSTGRES_URL").is_err() 
        && std::env::var("FORGEFLEET_DATABASE_URL").is_err() {
        return; // Prevents CI panic
    }
    // ... test logic
}
```

### Migrations
- **Forward-only only**: Add ONE new const per migration
- **Version registration**: Use next integer version
- **Never edit existing migrations**
- **Never add redundant migrations**

## Completion Protocol

1. Pass all formatting, static analysis, and targeted tests
2. **STOP immediately** after successful validation
3. Leave changes **uncommitted** in working tree
4. Do NOT run `git add`, `git commit`, or `git push`
5. Harness will commit/push uncommitted changes automatically

## Why This Prevents CI Failures

- Database tests panic in CI without URL checks
- Migration integrity breaks if existing migrations are modified
- Uncommitted changes bypass CI but get committed by harness

---

**Canary Marker**: <!-- 2026-07-28T21:14:23Z --> Local completion pipeline canary: one-line UTC timestamped marker validating the scheduler-driven local completion pipeline end-to-end; purpose is to confirm every pipeline stage executes and records lineage before merge.
