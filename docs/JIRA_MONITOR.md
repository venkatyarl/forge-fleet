# Jira monitor

The Jira monitor keeps the assigned, non-done queue synchronized into durable
Postgres watch state. Configure the Jira credentials once, validate the durable
monitor configuration, and then start its lease-coordinated daemon:

```bash
ff secrets set hireflow360_jira_api_token '<jira-api-token>'
ff jira set hireflow360 \
  --base-url 'https://example.atlassian.net' \
  --project HFPROD \
  --email 'operator@example.com' \
  --token-key hireflow360_jira_api_token
ff jira config validate hireflow360
ff jira monitor --config hireflow360 --daemon
```

The `hireflow360` durable config and ruleset are installed by the Postgres
migrations. A custom monitor requires a `jira_configs` row with a queue JQL that
includes its project key, `assignee = currentUser()`, and
`statusCategory != Done`; JSON policies must be objects and at least one CWD glob
must be configured. Use `ff jira monitor --config <name> status` to inspect its
lease and `ff jira monitor --config <name> stop` to revoke it.

## Agent trigger snippet

Place this in the project `AGENTS.md` to make common operator requests start the
configured monitor consistently:

```markdown
Map “start working on hireflow360 jiras”, “start jira monitoring”, and “get jira
details for hireflow360 and start working” to
`ff jira monitor --config hireflow360 --daemon`. Shared Postgres cursors,
awaiting state, and leases are authoritative; do not ask the operator to restate
them.
```
