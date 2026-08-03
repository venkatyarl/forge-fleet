# web-forge-fleet

The consolidated web presence + operations console for ForgeFleet — a Next.js
(App Router, TypeScript, Tailwind v4) static export served directly by
`ff-gateway`. This app replaces the old Vite `dashboard/` and the `web/` stub.

## What it is

- **Landing page (`/`)** — live fleet presence: hero, live stats, capability grid.
- **Console (`/mission-control` + ~30 routes)** — the full ops surface ported
  from the old dashboard: fleet overview, node detail, topology, mesh, pulse,
  model hub/inventory, LLM proxy, PM/work items, planning, workflows, brain +
  knowledge graph, agents, council, MCP, skills, interactions, metrics, alerts,
  audit log, cost ledger, versions, updates, settings, config editor, onboarding.
- **Workstreams (`/workstreams`)** — live session-of-record view (which
  claude/codex/kimi sessions are attached to each project), backed by
  `GET /api/workstreams` on the gateway.
- **Playground (`/playground`)** — streaming chat against the fleet's
  OpenAI-compatible proxy (`/v1/chat/completions`).

## Develop

```bash
npm install
npm run dev          # http://localhost:3000
```

In dev, `next.config.ts` rewrites proxy `/api/*`, `/v1/*`, `/mcp/*`, `/slm/*`
to the gateway (`http://127.0.0.1:8787` by default; override with
`FF_GATEWAY_URL`). WebSocket (`/ws`) and SSE (`/api/events/stream`) connect
directly to the gateway in dev — override with `NEXT_PUBLIC_FF_GATEWAY_URL`.

## Build & deploy

```bash
npm run build        # static export → out/
```

`ff-gateway` rust-embeds `out/` at compile time (`crates/ff-gateway/src/
static_files.rs`) and serves it as SPA statics with directory-index resolution
(`/fleet/` → `fleet/index.html`) and an `index.html` fallback — the same
single-binary deployment model as the old dashboard, with no Node server in
production. Rebuild the gateway after `npm run build` to bake the new assets.

## Conventions

- All data fetching is client-side (TanStack Query) against same-origin
  gateway endpoints — no server actions, no Next API routes, no SSR data.
- Pages are client components under `app/(console)/` (Header/Sidebar/
  CommandPalette shell); landing/workstreams/playground are standalone.
- Design tokens live in `app/globals.css` (Tailwind v4 CSS-first, dark
  zinc/violet theme, `localStorage('ff_dark_mode')` toggle).
- Dynamic console routes (`/nodes/[nodeId]`, `/brain/[threadSlug]`) use a
  placeholder `generateStaticParams` entry; the client router resolves real
  params at runtime (standard static-export SPA pattern).
