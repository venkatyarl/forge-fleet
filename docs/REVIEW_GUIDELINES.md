# PR Review Guidelines

## Required Review Keywords

Reviewers must include all applicable keywords in their approval comment.

| Keyword | Required When |
|---------|---------------|
| `schema-backward-compat` | The PR changes any database schema, table, index, migration, or persisted data format. |

## Schema Changes

Any pull request that modifies the database schema or persisted data formats must maintain **backward compatibility with at least one deployment generation**. This means a currently deployed version must continue to function against the new schema or data format during a rolling upgrade, or the change must be staged across two deployments (e.g., additive-only first, then cleanup in a subsequent deployment).

When `schema-backward-compat` applies, the reviewer should confirm:

- The change is additive-only, OR
- A two-step migration plan is documented and sequenced across deployments, OR
- A compatibility shim is in place for the previous deployment generation.
