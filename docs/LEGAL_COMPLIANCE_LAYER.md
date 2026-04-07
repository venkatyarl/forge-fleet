# Mission Control Legal/Compliance Layer (Phase 28)

ForgeFleet Mission Control now includes a lightweight legal/compliance domain in `ff-mc`:

- `legal_entities` — legal companies/org shells by jurisdiction
- `compliance_obligations` — recurring obligations (for example annual report, franchise tax)
- `filings` — due-dated filing records linked to entity (+ optional obligation)

## API surface

All endpoints are mounted under `/api/mc/legal`:

- Entities: `GET/POST /entities`, `GET/PATCH/DELETE /entities/{id}`
- Obligations: `GET/POST /obligations`, `GET/PATCH/DELETE /obligations/{id}`
- Filings: `GET/POST /filings`, `GET/PATCH/DELETE /filings/{id}`
- Deadline view: `GET /filings/due-soon?days=30` (default `30`, clamped `0..365`)

`due-soon` returns upcoming, unfiled items with enriched context (`entity_name`, `obligation_title`, `days_until_due`).
