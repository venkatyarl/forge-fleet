import { type ReactNode, useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'
import { extractNodes } from '../lib/normalizers'
import { FleetMemberModal } from '../components/FleetMemberModal'
import { ModelHub } from './ModelHub'
import { ToolInventory } from './ToolInventory'
import { AuditLog } from './AuditLog'
import { Updates } from './Updates'
import type { FleetNode, FleetStatusResponse } from '../types'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type RuntimeConfig = {
  loaded?: boolean
  config_path?: string | null
  fleet_name?: string
  api_port?: number
  heartbeat_interval_secs?: number
  heartbeat_timeout_secs?: number
  nodes_configured?: number
  models_configured?: number
  loops?: Record<string, boolean>
}

type TokenState = {
  configured_inline?: boolean
  env_var?: string
  resolved?: boolean
  source?: string
}

type EnrollmentSettings = {
  default_role?: string | null
  allowed_roles?: string[]
  token?: TokenState
}

type TelegramSettings = {
  configured?: boolean
  enabled?: boolean
  token?: TokenState
  runtime?: {
    running?: boolean
    last_error?: string | null
  }
}

type DatabaseSettings = {
  active_mode?: string
  status?: string
  sqlite?: { path?: string; file_exists?: boolean; wal_mode?: boolean }
  error?: string
}

type SettingsResponse = {
  runtime_config?: RuntimeConfig
  enrollment?: EnrollmentSettings
  telegram?: TelegramSettings
  database?: DatabaseSettings
}

// ---------------------------------------------------------------------------
// Settings tabs
// ---------------------------------------------------------------------------

type SettingsTab = 'general' | 'fleet' | 'memory' | 'models' | 'tools' | 'llm-proxy' | 'audit' | 'updates'

const TABS: { id: SettingsTab; label: string; icon: string }[] = [
  { id: 'general', label: 'General', icon: '⚙️' },
  { id: 'fleet', label: 'Fleet Members', icon: '🖥️' },
  { id: 'memory', label: 'Memory', icon: '🧠' },
  { id: 'models', label: 'Models', icon: '🤖' },
  { id: 'tools', label: 'Tools', icon: '🔧' },
  { id: 'llm-proxy', label: 'LLM Proxy', icon: '🔀' },
  { id: 'audit', label: 'Audit Log', icon: '📜' },
  { id: 'updates', label: 'Updates', icon: '🆙' },
]

// ---------------------------------------------------------------------------
// Main component
// ---------------------------------------------------------------------------

export function Settings() {
  const [activeTab, setActiveTab] = useState<SettingsTab>(() => {
    const hash = window.location.hash.replace('#', '') as SettingsTab
    return TABS.some(t => t.id === hash) ? hash : 'general'
  })

  // Update URL hash when tab changes
  useEffect(() => {
    window.location.hash = activeTab
  }, [activeTab])

  return (
    <section className="space-y-6">
      <h1 className="text-2xl font-bold text-zinc-100">Settings</h1>

      {/* Tab navigation */}
      <div className="flex flex-wrap gap-1 rounded-lg border border-zinc-800 bg-zinc-900/50 p-1">
        {TABS.map(tab => (
          <button
            key={tab.id}
            onClick={() => setActiveTab(tab.id)}
            className={`flex items-center gap-1.5 rounded-md px-3 py-2 text-sm transition ${
              activeTab === tab.id
                ? 'bg-violet-500/20 text-violet-300 font-medium'
                : 'text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200'
            }`}
          >
            <span className="text-xs">{tab.icon}</span>
            <span>{tab.label}</span>
          </button>
        ))}
      </div>

      {/* Tab content */}
      {activeTab === 'general' && <GeneralTab />}
      {activeTab === 'fleet' && <FleetMembersTab />}
      {activeTab === 'memory' && <MemoryTab />}
      {activeTab === 'models' && <ModelsTab />}
      {activeTab === 'tools' && <ToolsTab />}
      {activeTab === 'llm-proxy' && <LLMProxyTab />}
      {activeTab === 'audit' && <AuditTab />}
      {activeTab === 'updates' && <UpdatesTab />}
    </section>
  )
}

// ---------------------------------------------------------------------------
// General Tab
// ---------------------------------------------------------------------------

function GeneralTab() {
  const [data, setData] = useState<SettingsResponse | null>(null)
  const [loading, setLoading] = useState(true)

  const load = useCallback(async () => {
    setLoading(true)
    try {
      const payload = await getJson<SettingsResponse>('/api/settings/runtime')
      setData(payload)
    } catch { /* ignore */ }
    setLoading(false)
  }, [])

  useEffect(() => { void load() }, [load])

  const runtime = data?.runtime_config
  const enrollment = data?.enrollment
  const telegram = data?.telegram
  const database = data?.database

  if (loading) return <LoadingPlaceholder />

  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card title="Runtime Config" status={<Pill ok={Boolean(runtime?.loaded)} okLabel="loaded" badLabel="not loaded" />}>
        <Row label="Config path" value={runtime?.config_path} />
        <Row label="Fleet name" value={runtime?.fleet_name} />
        <Row label="API port" value={runtime?.api_port} />
        <Row label="Heartbeat interval" value={runtime?.heartbeat_interval_secs != null ? `${runtime.heartbeat_interval_secs}s` : undefined} />
        <Row label="Nodes configured" value={runtime?.nodes_configured} />
        <Row label="Models configured" value={runtime?.models_configured} />
        {runtime?.loops && Object.keys(runtime.loops).length > 0 && (
          <div className="mt-3 grid grid-cols-2 gap-1.5">
            {Object.entries(runtime.loops).map(([name, enabled]) => (
              <div key={name} className="flex items-center justify-between rounded border border-zinc-800 px-2 py-1 text-xs">
                <span className="text-zinc-400">{name}</span>
                <Pill ok={Boolean(enabled)} okLabel="on" badLabel="off" />
              </div>
            ))}
          </div>
        )}
      </Card>

      <Card title="Enrollment" status={<Pill ok={Boolean(enrollment?.token?.resolved)} okLabel="healthy" badLabel="needs setup" />}>
        <Row label="Default role" value={enrollment?.default_role ?? 'worker'} />
        <Row label="Token source" value={enrollment?.token?.source} />
        <Row label="Token resolved" value={enrollment?.token?.resolved ? 'yes' : 'no'} />
      </Card>

      <Card title="Telegram Transport" status={<Pill ok={Boolean(telegram?.enabled && telegram?.runtime?.running)} okLabel="running" badLabel="inactive" />}>
        <Row label="Enabled" value={telegram?.enabled ? 'yes' : 'no'} />
        <Row label="Token resolved" value={telegram?.token?.resolved ? 'yes' : 'no'} />
        <Row label="Runtime running" value={telegram?.runtime?.running ? 'yes' : 'no'} />
        {telegram?.runtime?.last_error && <Row label="Last error" value={telegram.runtime.last_error} />}
      </Card>

      <Card title="Database" status={<Pill ok={database?.status === 'ready'} okLabel="ready" badLabel={database?.status ?? 'unknown'} />}>
        <Row label="Active mode" value={database?.active_mode} />
        <Row label="Status" value={database?.status} />
        {database?.sqlite && <Row label="SQLite path" value={database.sqlite.path} />}
        {database?.error && <Row label="Error" value={database.error} />}
      </Card>
    </div>
  )
}

// ---------------------------------------------------------------------------
// Fleet Members Tab
// ---------------------------------------------------------------------------

function FleetMembersTab() {
  const [members, setMembers] = useState<FleetNode[]>([])
  const [loading, setLoading] = useState(true)
  const [selectedMember, setSelectedMember] = useState<FleetNode | null>(null)

  const load = useCallback(async () => {
    setLoading(true)
    try {
      const fleet = await getJson<FleetStatusResponse>('/api/fleet/status').catch(() => getJson<FleetStatusResponse>('/api/status'))
      const nodes = extractNodes(fleet)
      // Filter out phantom/test nodes
      const realMembers = nodes.filter(n => {
        const name = (n.name ?? '').toLowerCase()
        return !name.includes('postgres-verify') && !name.includes('verify.local')
      })
      // Default all to offline, then verify with real health checks
      const membersWithStatus = realMembers.map(m => ({ ...m, status: 'offline' as const }))
      setMembers(membersWithStatus)

      // Live health check — ping each node's daemon port (51000)
      for (const member of membersWithStatus) {
        const ip = member.ip
        if (!ip) continue
        // Try the LLM health endpoint (port 51000)
        fetch(`http://${ip}:51000/health`, { signal: AbortSignal.timeout(3000) })
          .then(r => {
            if (r.ok) {
              setMembers(prev => prev.map(m =>
                m.name === member.name ? { ...m, status: 'online' } : m
              ))
            }
          })
          .catch(() => {
            // Also try port 51002 (gateway/web UI)
            fetch(`http://${ip}:51002/health`, { signal: AbortSignal.timeout(3000) })
              .then(r => {
                if (r.ok) {
                  setMembers(prev => prev.map(m =>
                    m.name === member.name ? { ...m, status: 'online' } : m
                  ))
                }
              })
              .catch(() => { /* truly offline */ })
          })
      }
    } catch { /* ignore */ }
    setLoading(false)
  }, [])

  useEffect(() => { void load() }, [load])

  if (loading) return <LoadingPlaceholder />

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <p className="text-sm text-zinc-500">{members.length} fleet member{members.length !== 1 ? 's' : ''}</p>
        <a href="/onboarding" className="rounded-md border border-violet-500/40 bg-violet-500/15 px-3 py-1.5 text-sm text-violet-300 hover:bg-violet-500/25 transition">
          + Add New Member
        </a>
      </div>

      <div className="space-y-3">
        {members.map(member => (
          <FleetMemberRow key={member.id ?? member.name} member={member} onClick={() => setSelectedMember(member)} />
        ))}
      </div>

      {members.length === 0 && (
        <div className="rounded-xl border border-zinc-800 bg-zinc-900/50 p-8 text-center">
          <p className="text-lg text-zinc-400">No fleet members connected</p>
          <p className="mt-1 text-sm text-zinc-500">Add your first fleet member to get started</p>
        </div>
      )}

      <FleetMemberModal member={selectedMember} onClose={() => setSelectedMember(null)} />
    </div>
  )
}

function FleetMemberRow({ member, onClick }: { member: FleetNode; onClick: () => void }) {
  const status = (member.status ?? member.health ?? 'unknown').toLowerCase()
  const isLeader = member.is_leader || member.leader_state === 'leader'
  const models = member.models_loaded ?? (member.models ?? []).map(m => m.name)

  return (
    <button
      onClick={onClick}
      className="w-full text-left rounded-xl border border-zinc-800 bg-zinc-900/60 p-4 transition hover:border-zinc-700 hover:bg-zinc-900"
    >
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <span className={`h-2.5 w-2.5 rounded-full flex-shrink-0 ${
            status === 'online' ? 'bg-emerald-400' : status === 'degraded' ? 'bg-amber-400' : 'bg-zinc-600'
          }`} />
          <div>
            <span className="font-medium text-zinc-200 capitalize">{member.name}</span>
            <span className="ml-2 text-sm text-zinc-500">{member.ip ?? 'unknown IP'}</span>
            {isLeader && <span className="ml-2 rounded-full bg-violet-500/20 px-2 py-0.5 text-[10px] text-violet-300">leader</span>}
          </div>
        </div>
        <div className="flex items-center gap-3 text-sm text-zinc-500">
          <span>{member.cpu ?? member.hardware?.cpu ?? '—'}</span>
          <span>{member.ram ?? member.hardware?.ram ?? '—'}</span>
        </div>
      </div>

      {/* Models running on this member */}
      {models.length > 0 && (
        <div className="mt-3 flex flex-wrap gap-2">
          {models.map(name => (
            <span key={name} className="rounded-md border border-zinc-700 bg-zinc-800/80 px-2 py-1 text-xs text-zinc-300">
              🤖 {name}
            </span>
          ))}
        </div>
      )}
      {models.length === 0 && (
        <p className="mt-2 text-xs text-zinc-600">No models reported</p>
      )}
    </button>
  )
}

// ---------------------------------------------------------------------------
// Placeholder tabs (delegate to existing pages)
// ---------------------------------------------------------------------------

function MemoryTab() {
  const [brain, setBrain] = useState<{
    project?: { name?: string; root?: string; entries?: number; has_forgefleet_md?: boolean; has_context_md?: boolean }
    brain?: { entries?: number; has_brain_md?: boolean }
    hive?: { entries?: number; has_hive_md?: boolean }
  } | null>(null)
  const [loading, setLoading] = useState(true)

  const load = useCallback(async () => {
    setLoading(true)
    try {
      const data = await getJson<typeof brain>('/api/brain/status')
      setBrain(data)
    } catch { /* ignore */ }
    setLoading(false)
  }, [])

  useEffect(() => { void load() }, [load])

  if (loading) return <LoadingPlaceholder />

  const p = brain?.project
  const b = brain?.brain
  const h = brain?.hive

  return (
    <div className="space-y-6">
      <p className="text-sm text-zinc-500">
        ForgeFleet uses three memory layers: Project Memory (per-project), Fleet Brain (personal), and Hive Mind (shared across fleet).
      </p>

      <div className="grid gap-4 lg:grid-cols-3">
        {/* Project Memory */}
        <Card title="Project Memory" status={<Pill ok={(p?.entries ?? 0) > 0} okLabel={`${p?.entries ?? 0} entries`} badLabel="empty" />}>
          <Row label="Project" value={p?.name ?? 'none detected'} />
          <Row label="Root" value={p?.root ?? 'not in a project'} />
          <Row label="FORGEFLEET.md" value={p?.has_forgefleet_md ? 'yes' : 'no'} />
          <Row label="context.md" value={p?.has_context_md ? 'yes' : 'no'} />
          <div className="mt-3 text-xs text-zinc-500">
            Lives at <code className="text-violet-300">{'{'}project{'}'}.forgefleet/</code> — committed to git, travels with code.
          </div>
        </Card>

        {/* Fleet Brain */}
        <Card title="Fleet Brain" status={<Pill ok={(b?.entries ?? 0) > 0} okLabel={`${b?.entries ?? 0} entries`} badLabel="empty" />}>
          <Row label="BRAIN.md" value={b?.has_brain_md ? 'yes' : 'no'} />
          <Row label="Entries" value={b?.entries ?? 0} />
          <div className="mt-3 text-xs text-zinc-500">
            Personal preferences at <code className="text-violet-300">~/.forgefleet/brain/</code> — never synced to other fleet members.
          </div>
        </Card>

        {/* Hive Mind */}
        <Card title="Hive Mind" status={<Pill ok={(h?.entries ?? 0) > 0} okLabel={`${h?.entries ?? 0} entries`} badLabel="empty" />}>
          <Row label="HIVE.md" value={h?.has_hive_md ? 'yes' : 'no'} />
          <Row label="Entries" value={h?.entries ?? 0} />
          <div className="mt-3 text-xs text-zinc-500">
            Shared standards at <code className="text-violet-300">~/.forgefleet/hive/</code> — synced across all fleet members via git.
          </div>
        </Card>
      </div>
    </div>
  )
}

function ModelsTab() {
  return <ModelHub />
}

function ToolsTab() {
  return <ToolInventory />
}

function LLMProxyTab() {
  return <EmbedMessage title="LLM Proxy" description="Route LLM requests across fleet members for load balancing." linkTo="/llm-proxy" linkLabel="Open LLM Proxy" />
}

function AuditTab() {
  return <AuditLog />
}

function UpdatesTab() {
  return <Updates />
}

function EmbedMessage({ title, description, linkTo, linkLabel }: { title: string; description: string; linkTo: string; linkLabel: string }) {
  return (
    <div className="rounded-xl border border-zinc-800 bg-zinc-900/50 p-8 text-center">
      <h3 className="text-lg font-medium text-zinc-200">{title}</h3>
      <p className="mt-1 text-sm text-zinc-500">{description}</p>
      <a href={linkTo} className="mt-4 inline-block rounded-md bg-violet-500/20 px-4 py-2 text-sm text-violet-300 hover:bg-violet-500/30 transition">
        {linkLabel}
      </a>
    </div>
  )
}

// ---------------------------------------------------------------------------
// Shared components
// ---------------------------------------------------------------------------

function Card({ title, status, children }: { title: string; status?: ReactNode; children: ReactNode }) {
  return (
    <article className="rounded-xl border border-zinc-800 bg-zinc-900/60 p-4">
      <div className="mb-3 flex items-center justify-between gap-3">
        <h2 className="text-sm font-semibold uppercase tracking-wide text-zinc-400">{title}</h2>
        {status}
      </div>
      <div className="space-y-2">{children}</div>
    </article>
  )
}

function Row({ label, value }: { label: string; value: unknown }) {
  const display = value == null || value === '' ? 'unreported' : String(value)
  return (
    <div className="flex items-start justify-between gap-3 border-b border-zinc-800/60 pb-2 text-sm last:border-b-0 last:pb-0">
      <dt className="text-zinc-500">{label}</dt>
      <dd className="text-right text-zinc-200">{display}</dd>
    </div>
  )
}

function Pill({ ok, okLabel, badLabel }: { ok: boolean; okLabel: string; badLabel: string }) {
  return (
    <span className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${
      ok ? 'bg-emerald-500/20 text-emerald-300' : 'bg-amber-500/20 text-amber-300'
    }`}>
      {ok ? okLabel : badLabel}
    </span>
  )
}

function LoadingPlaceholder() {
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      {[1,2,3,4].map(i => (
        <div key={i} className="h-40 animate-pulse rounded-xl border border-zinc-800 bg-zinc-900/30" />
      ))}
    </div>
  )
}
