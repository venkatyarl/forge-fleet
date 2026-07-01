# ForgeFleet UI Ground-Up Redesign Plan

> Scope: rebuild `crates/ff-terminal/` (TUI) and `dashboard/` (web) to be fast,
> beautiful, stable, and feature-parity by construction.
>
> **Current decision:** web dashboard first, then mirror to TUI.

---

## 1. Current State

### Web dashboard (`dashboard/`)

- Stack: React 19, Vite, TypeScript, `react-router-dom` v7, Tailwind CSS v3.
- 28 page files under `src/pages/`, all backed by `ff-gateway` REST + WebSocket.
- Components are mostly domain-specific panels; reusable primitives are limited.
- No global React Context; state is local to pages/hooks.
- Dark mode via `dark` class + hand-written Tailwind classes.
- Command palette exists but uses hard-coded navigation commands.
- WebSocket feed and SSE events are consumed, but each page manages its own
  refresh/loading state.

### TUI (`crates/ff-terminal/`)

- ratatui 0.29 + crossterm 0.29.
- One big `App` struct with a single `scroll_offset` on the active session tab.
- The left sidebar is rendered as one monolithic `Paragraph`, so **no pane can
  scroll independently**.
- The footer builds a single `Line` of `Span`s and is prone to truncation /
  overlap on narrow terminals.
- Agent work appears to run inline; long "Thinking…" turns can freeze input.
- Fleet topology, task queue, focus stack, backlog, and fleet list are all
  crammed into the same scrolling sidebar.

---

## 2. Research Findings

### What great coding/agent TUIs do

- **Independent viewports:** lazygit, k9s, lazygit, Zellij, Charm Bubble Tea
  keep each pane in its own scroll state; the frame never jumps.
- **Non-blocking event loops:** input, render, and background work run on
  separate tasks/channels; the UI stays interactive while work is in flight.
- **Resize safety:** layouts recompute from the terminal size every frame;
  footer/header use constrained sub-layouts, not one long line.
- **Keyboard-first discoverability:** a global command palette + context-aware
  keybinding help is mandatory.

### What great AI dashboards do

- **Streaming chat with tool cards:** messages show intermediate tool calls,
  reasoning blocks, and artifacts.
- **Command palette (⌘K):** route jumps, actions, and feature commands in one
  surface.
- **Dense data tables:** virtualized tables, status pills, sparklines, and
  filter bars (TanStack Table + Virtual).
- **Live state via WebSocket deltas:** no polling; state is a projection of a
  snapshot + ordered events.
- **Bento / pane layout:** mission control uses modular cards that can be
  scanned at a glance.

---

## 3. `ff council` Architecture Consensus

Convened: codex + kimi (chair: codex).

### Agreed decisions

1. **Rust owns the contract.** A shared crate/protocol defines
   `DashboardSnapshot`, `DashboardEvent`, `CommandId`, `RouteId`, and the
   capability manifest. TypeScript types are generated (`ts-rs` / `typeshare`).
2. **Frontend state model:** `snapshot + ordered events -> projected state`.
   - Server state: **TanStack Query**.
   - Real-time updates: one typed `/ws` client with reconnect, heartbeat,
     `seq`/`event_id`, gap detection, and `ResyncRequired` snapshot reload.
   - Local UI state only: **Zustand** (palette open, focused pane, selected row,
     density, modals).
3. **Navigation / commands:** global command palette via `cmdk`; keyboard
   manager via `react-hotkeys-hook`; route/command registry derives from the
   shared capability manifest.
4. **Component structure:**
   - `src/protocol/` — generated types + runtime validators.
   - `src/sync/` — WebSocket client, event reducers, resync logic.
   - `src/app/` — providers, router, command registry, keybindings.
   - `src/features/` — fleet, agents, llms, work-items, logs, settings, chat.
   - `src/components/ui/` — design-system primitives.
   - `src/components/data/` — virtualized tables, lists, status cells.
5. **UI stack:** Tailwind CSS with OKLCH dark tokens, TanStack Table, TanStack
   Virtual, `lucide-react`, Radix primitives or local shadcn-style primitives.

### Dissent resolved

Kimi favored a normalized Zustand entity cache; Codex favored keeping fleet
state in TanStack Query. Consensus: **TanStack Query is the server-state
authority**; normalization lives inside Query data/selectors if needed.

### Adaptation for this project

The existing dashboard already uses React Router 7, so we will **adopt the
state/command/data layers of the consensus while keeping React Router** rather
than swapping in TanStack Router. Route IDs and command IDs are still shared
with the TUI via the Rust contract, and a future migration to TanStack Router
remains possible without touching the state layer.

---

## 4. Shared State Contract (TUI ↔ Web Parity)

A new crate `crates/ff-dashboard-core/` (or module inside `ff-core`) defines:

```rust
pub struct DashboardSnapshot {
    pub fleet: Vec<FleetNode>,
    pub llm_servers: Vec<LlmServer>,
    pub work_items: Vec<WorkItem>,
    pub agents: Vec<AgentRun>,
    pub sessions: Vec<SessionSummary>,
    pub alerts: Vec<Alert>,
    pub skills: Vec<Skill>,
    pub capabilities: CapabilityManifest,
    pub me: OperatorContext,
}

pub enum DashboardEvent {
    FleetUpdated { seq: u64, node: FleetNode },
    LlmServerUpdated { seq: u64, server: LlmServer },
    WorkItemChanged { seq: u64, item: WorkItem },
    AgentEvent { seq: u64, run_id: String, event: AgentEvent },
    AlertFired { seq: u64, alert: Alert },
    AlertCleared { seq: u64, alert_id: String },
    Heartbeat,
    ResyncRequired,
}

pub struct CapabilityManifest {
    pub routes: Vec<RouteDef>,
    pub commands: Vec<CommandDef>,
}
```

Both the TUI and the web dashboard consume the same snapshot + event stream.
Feature parity is enforced by the backend emitting the same events and the same
capability manifest; each UI only decides how to render them.

---

## 5. Web Dashboard Implementation Plan

### Phase W1 — Foundation

1. Add dependencies:
   - `@tanstack/react-query`
   - `zustand`
   - `cmdk`
   - `react-hotkeys-hook`
   - `@tanstack/react-table`
   - `@tanstack/react-virtual`
   - `date-fns`
   - `lucide-react` (if not present)
   - `class-variance-authority`, `clsx`, `tailwind-merge`
2. Replace `tailwind.config.js` with a design-token file:
   - OKLCH dark palette (`--color-bg`, `--color-surface`, `--color-panel`,
     `--color-accent`, semantic status colors).
   - Density scale (compact / normal).
   - Transition/animation tokens.
3. Create `src/protocol/` with generated TS types and hand-written validators
   for the initial contract.
4. Create `src/sync/`:
   - `ws-client.ts` — typed WebSocket client, reconnect, heartbeat, seq tracking.
   - `events.ts` — event reducer that updates TanStack Query cache.
5. Create `src/app/providers.tsx` wrapping React Query + router + theme.

### Phase W2 — Shell + Command Palette + Navigation

1. Redesign `Shell`:
   - Collapsible sidebar with sections matching `CapabilityManifest.routes`.
   - Top `Header` with live connection badge, event ticker, search trigger.
   - Bento-style home (`MissionControl`) with live cards.
2. Build a global `CommandPalette` using `cmdk` driven by the shared command
   registry.
3. Wire `react-hotkeys-hook` to route jumps, command palette, and pane focus.
4. Add density toggle and theme persistence.

### Phase W3 — Chat + Work + Fleet Views

1. Rebuild the chat page (`Brain`) as a first-class streaming conversation
   surface:
   - message bubbles with markdown/code blocks/tool cards,
   - focus stack and backlog side panels,
   - live tool-status footer,
   - independent scroll panes for chat/context.
2. Add a live `WorkBoard` (work_items by status with assignee/host):
   - source from `/api/mc/work-items` + WebSocket `WorkItemChanged`.
3. Redesign `FleetOverview`/`Topology`/`ModelHub` with:
   - virtualized tables,
   - status pills,
   - GPU sparklines,
   - topology graph (SVG/Canvas).

### Phase W4 — Remaining Pages + Polish

1. Apply the new design system to every existing page.
2. Fill gaps identified in inventory (unified skills browser, swarm launch,
   council view, MCP manager, memory/brain browser).
3. Add empty states, skeletons, error boundaries, and keyboard help.
4. Ensure `npm run build` passes (tsc + vite).

---

## 6. TUI Implementation Plan (after web lands)

1. Introduce the same `DashboardSnapshot`/`DashboardEvent` contract; replace
   ad-hoc DB queries with gateway snapshot + `/ws` events.
2. Split the monolithic sidebar into independent panes with their own
   `ScrollableState`.
3. Rewrite the event loop:
   - crossterm events -> input queue,
   - background worker for agent/LLM/tool calls -> event channel,
   - render loop never blocks on work.
4. Rebuild footer as a constrained sub-layout with truncation-safe fields.
5. Add command palette and keybinding help.
6. Resize/SIGWINCH handled cleanly; test on narrow and wide terminals.
7. Verify `cargo +1.88.0 fmt --check` + `cargo check`.

---

## 7. Verification & Deliverables

- `cargo +1.88.0 fmt --check && cargo check` (TUI)
- `npm run build` (web)
- `plans/ui-redesign.md` (this file) updated as decisions evolve
- `docs/ui-redesign-whatsnew.md` short migration note
- Open PRs; let CI gate them

---

## 8. Open Questions / Next Council Topics

- Should the shared contract live in a new `ff-dashboard-core` crate or inside
  `ff-core`?
- Should we keep the current `/ws` event shape or migrate gateway to emit the
  new `DashboardEvent` envelope immediately?
- Which pages should be combined or split (e.g., `Brain` vs dedicated
  `Council`, `Skills`, `Swarm` pages)?
