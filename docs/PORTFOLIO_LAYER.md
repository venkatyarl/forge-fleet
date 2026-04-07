# Portfolio Layer (Mission Control)

ForgeFleet’s portfolio/company/project operating model layer now lives in:

- `crates/ff-mc/src/portfolio.rs` — domain models + persistence logic
- `crates/ff-mc/src/db.rs` — SQLite schema/migration for portfolio tables
- `crates/ff-mc/src/api.rs` — REST CRUD endpoints + portfolio summary endpoint

## Persisted tables

- `companies`
- `projects`
- `project_repos`
- `project_environments`

`business_unit` is modeled as a company field (`companies.business_unit`) instead of a separate `business_units` table.

## API surface

- `GET/POST /api/mc/companies`
- `GET/PATCH/DELETE /api/mc/companies/{id}`
- `GET/POST /api/mc/projects`
- `GET/PATCH/DELETE /api/mc/projects/{id}`
- `GET/POST /api/mc/projects/{id}/repos`
- `GET/PATCH/DELETE /api/mc/project-repos/{id}`
- `GET/POST /api/mc/projects/{id}/environments`
- `GET/PATCH/DELETE /api/mc/project-environments/{id}`
- `GET /api/mc/portfolio/summary`

## Key operating fields

Company and project records include:

- `status`
- `priority`
- `owner`
- `operating_stage`
- `compliance_sensitivity`
- `revenue_model_tags`
