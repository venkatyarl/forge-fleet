import { type ReactNode, useCallback, useEffect, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { getJson } from '../lib/api'
import { extractNodes } from '../lib/normalizers'
import { FleetMemberModal } from '../components/FleetMemberModal'
import { ModelHub } from './ModelHub'
import { ToolInventory } from './ToolInventory'
import { AuditLog } from './AuditLog'
import { Updates } from './Updates'
import { cn } from '../lib/utils'
import type { FleetComputer, FleetStatusResponse } from '../types'

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
  { id: 'updates', label: 'Updates', icon: '🔄' },
  { id: 'fleet', label: 'Health', icon: '🖥️' },
  { id: 'memory', label: 'Brain', icon: '🧠' },
  { id: 'audit', label: 'Logs', icon: '📜' },
  { id: 'models', label: 'Models', icon: '🤖' },
  { id: 'tools', label: 'Tools', icon: '🔧' },
  { id: 'llm-proxy', label: 'LLM Proxy', icon: '🔀' },
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
    <section className="min-h-full space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <h1 className="text-2xl font-semibold text-foreground">Settings</h1>
          <p className="mt-1 text-sm text-muted">
            Runtime configuration, fleet health, memory layers, updates, and operational logs.
          </p>
        </div>
        <Badge variant="default">ForgeFleet</Badge>
      </div>

      {/* Tab navigation */}
      <div className="flex flex-wrap gap-1 rounded-xl border border-border bg-panel p-1">
        {TABS.map(tab => (
          <Button
            key={tab.id}
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => setActiveTab(tab.id)}
            className={cn(
              'h-8 rounded-lg px-3 text-muted hover:bg-elevated hover:text-foreground',
              activeTab === tab.id && 'bg-primary-subtle text-primary hover:bg-primary-subtle hover:text-primary'
            )}
          >
            <span className="text-xs">{tab.icon}</span>
            <span>{tab.label}</span>
          </Button>
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
      <SettingsCard
        title="Runtime Config"
        description="Loaded runtime values from /api/settings/runtime."
        status={<StatusBadge status={runtime?.loaded ? 'ready' : 'warning'}>{runtime?.loaded ? 'loaded' : 'not loaded'}</StatusBadge>}
      >
        <Row label="Config path" value={runtime?.config_path} />
        <Row label="Fleet name" value={runtime?.fleet_name} />
        <Row label="API port" value={runtime?.api_port} />
        <Row label="Heartbeat interval" value={runtime?.heartbeat_interval_secs != null ? `${runtime.heartbeat_interval_secs}s` : undefined} />
        <Row label="Nodes configured" value={runtime?.nodes_configured} />
        <Row label="Models configured" value={runtime?.models_configured} />
        {runtime?.loops && Object.keys(runtime.loops).length > 0 && (
          <div className="mt-3 grid grid-cols-2 gap-1.5">
            {Object.entries(runtime.loops).map(([name, enabled]) => (
              <div key={name} className="flex items-center justify-between rounded-lg border border-border bg-surface px-2 py-1 text-xs">
                <span className="text-muted">{name}</span>
                <Badge variant={enabled ? 'ok' : 'neutral'}>{enabled ? 'on' : 'off'}</Badge>
              </div>
            ))}
          </div>
        )}
      </SettingsCard>

      <SettingsCard
        title="Enrollment"
        description="Default member role and enrollment token resolution."
        status={<StatusBadge status={enrollment?.token?.resolved ? 'healthy' : 'warning'}>{enrollment?.token?.resolved ? 'healthy' : 'needs setup'}</StatusBadge>}
      >
        <Row label="Default role" value={enrollment?.default_role ?? 'worker'} />
        <Row label="Token source" value={enrollment?.token?.source} />
        <Row label="Token resolved" value={enrollment?.token?.resolved ? 'yes' : 'no'} />
      </SettingsCard>

      <SettingsCard
        title="Telegram Transport"
        description="Transport token and runtime process status."
        status={<StatusBadge status={telegram?.enabled && telegram?.runtime?.running ? 'running' : 'standby'}>{telegram?.enabled && telegram?.runtime?.running ? 'running' : 'inactive'}</StatusBadge>}
      >
        <Row label="Enabled" value={telegram?.enabled ? 'yes' : 'no'} />
        <Row label="Token resolved" value={telegram?.token?.resolved ? 'yes' : 'no'} />
        <Row label="Runtime running" value={telegram?.runtime?.running ? 'yes' : 'no'} />
        {telegram?.runtime?.last_error && <Row label="Last error" value={telegram.runtime.last_error} />}
      </SettingsCard>

      <SettingsCard
        title="Database"
        description="Active persistence backend reported by the gateway."
        status={<StatusBadge status={database?.status === 'ready' ? 'ready' : database?.status ?? 'unknown'}>{database?.status === 'ready' ? 'ready' : database?.status ?? 'unknown'}</StatusBadge>}
      >
        <Row label="Active mode" value={database?.active_mode} />
        <Row label="Status" value={database?.status} />
        {database?.sqlite && <Row label="SQLite path" value={database.sqlite.path} />}
        {database?.error && <Row label="Error" value={database.error} />}
      </SettingsCard>
    </div>
  )
}

// ---------------------------------------------------------------------------
// Fleet Members Tab
// ---------------------------------------------------------------------------

function FleetMembersTab() {
  const [members, setMembers] = useState<FleetComputer[]>([])
  const [loading, setLoading] = useState(true)
  const [selectedMember, setSelectedMember] = useState<FleetComputer | null>(null)

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
        <p className="text-sm text-muted">{members.length} fleet member{members.length !== 1 ? 's' : ''}</p>
        <Button type="button" variant="outline" onClick={() => { window.location.href = '/onboarding' }}>
          + Add New Member
        </Button>
      </div>

      <div className="space-y-3">
        {members.map(member => (
          <FleetMemberRow key={member.id ?? member.name} member={member} onClick={() => setSelectedMember(member)} />
        ))}
      </div>

      {members.length === 0 && (
        <Card className="bg-panel p-8 text-center">
          <CardTitle className="text-base">No fleet members connected</CardTitle>
          <CardDescription className="mt-1">Add your first fleet member to get started.</CardDescription>
        </Card>
      )}

      <FleetMemberModal member={selectedMember} onClose={() => setSelectedMember(null)} />
    </div>
  )
}

function FleetMemberRow({ member, onClick }: { member: FleetComputer; onClick: () => void }) {
  const status = (member.status ?? member.health ?? 'unknown').toLowerCase()
  const isLeader = member.is_leader || member.leader_state === 'leader'
  const models = member.models_loaded ?? (member.models ?? []).map(m => m.name)

  return (
    <button
      onClick={onClick}
      className="w-full rounded-xl border border-border bg-panel p-4 text-left transition hover:border-border-subtle hover:bg-elevated"
    >
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <span className={cn(
            'h-2.5 w-2.5 flex-shrink-0 rounded-full',
            status === 'online' ? 'bg-status-ok' : status === 'degraded' ? 'bg-status-warn' : 'bg-border-subtle'
          )} />
          <div>
            <span className="font-medium capitalize text-foreground">{member.name}</span>
            <span className="ml-2 text-sm text-dim">{member.ip ?? 'unknown IP'}</span>
            {isLeader && <Badge className="ml-2" variant="default">leader</Badge>}
          </div>
        </div>
        <div className="flex items-center gap-3 text-sm text-muted">
          <StatusBadge status={status}>{status}</StatusBadge>
          <span>{member.cpu ?? member.hardware?.cpu ?? '—'}</span>
          <span>{member.ram ?? member.hardware?.ram ?? '—'}</span>
        </div>
      </div>

      {/* Models running on this member */}
      {models.length > 0 && (
        <div className="mt-3 flex flex-wrap gap-2">
          {models.map(name => (
            <Badge key={name} variant="neutral">🤖 {name}</Badge>
          ))}
        </div>
      )}
      {models.length === 0 && (
        <p className="mt-2 text-xs text-dim">No models reported</p>
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
      <p className="text-sm text-muted">
        ForgeFleet uses three memory layers: Project Memory (per-project), Fleet Brain (personal), and Hive Mind (shared across fleet).
      </p>

      <div className="grid gap-4 lg:grid-cols-3">
        {/* Project Memory */}
        <SettingsCard
          title="Project Memory"
          description="Repository-scoped context that travels with code."
          status={<StatusBadge status={(p?.entries ?? 0) > 0 ? 'active' : 'standby'}>{(p?.entries ?? 0) > 0 ? `${p?.entries ?? 0} entries` : 'empty'}</StatusBadge>}
        >
          <Row label="Project" value={p?.name ?? 'none detected'} />
          <Row label="Root" value={p?.root ?? 'not in a project'} />
          <Row label="FORGEFLEET.md" value={p?.has_forgefleet_md ? 'yes' : 'no'} />
          <Row label="context.md" value={p?.has_context_md ? 'yes' : 'no'} />
          <div className="mt-3 text-xs text-muted">
            Lives at <code className="rounded bg-primary-subtle px-1 text-primary">{'{'}project{'}'}.forgefleet/</code> — committed to git, travels with code.
          </div>
        </SettingsCard>

        {/* Fleet Brain */}
        <SettingsCard
          title="Fleet Brain"
          description="Personal operator memory local to this fleet."
          status={<StatusBadge status={(b?.entries ?? 0) > 0 ? 'active' : 'standby'}>{(b?.entries ?? 0) > 0 ? `${b?.entries ?? 0} entries` : 'empty'}</StatusBadge>}
        >
          <Row label="BRAIN.md" value={b?.has_brain_md ? 'yes' : 'no'} />
          <Row label="Entries" value={b?.entries ?? 0} />
          <div className="mt-3 text-xs text-muted">
            Personal preferences at <code className="rounded bg-primary-subtle px-1 text-primary">~/.forgefleet/brain/</code> — never synced to other fleet members.
          </div>
        </SettingsCard>

        {/* Hive Mind */}
        <SettingsCard
          title="Hive Mind"
          description="Shared standards synchronized across the fleet."
          status={<StatusBadge status={(h?.entries ?? 0) > 0 ? 'active' : 'standby'}>{(h?.entries ?? 0) > 0 ? `${h?.entries ?? 0} entries` : 'empty'}</StatusBadge>}
        >
          <Row label="HIVE.md" value={h?.has_hive_md ? 'yes' : 'no'} />
          <Row label="Entries" value={h?.entries ?? 0} />
          <div className="mt-3 text-xs text-muted">
            Shared standards at <code className="rounded bg-primary-subtle px-1 text-primary">~/.forgefleet/hive/</code> — synced across all fleet members via git.
          </div>
        </SettingsCard>
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
    <Card className="bg-panel p-8 text-center">
      <CardTitle className="text-lg">{title}</CardTitle>
      <CardDescription className="mt-1">{description}</CardDescription>
      <Button type="button" className="mt-4" onClick={() => { window.location.href = linkTo }}>
        {linkLabel}
      </Button>
    </Card>
  )
}

// ---------------------------------------------------------------------------
// Shared components
// ---------------------------------------------------------------------------

function SettingsCard({ title, description, status, children }: { title: string; description?: string; status?: ReactNode; children: ReactNode }) {
  return (
    <Card className="bg-panel">
      <CardHeader>
        <div>
          <CardTitle>{title}</CardTitle>
          {description ? <CardDescription>{description}</CardDescription> : null}
        </div>
        {status}
      </CardHeader>
      <div className="space-y-2">{children}</div>
    </Card>
  )
}

function Row({ label, value }: { label: string; value: unknown }) {
  const display = value == null || value === '' ? 'unreported' : String(value)
  return (
    <div className="flex items-start justify-between gap-3 border-b border-border pb-2 text-sm last:border-b-0 last:pb-0">
      <dt className="text-muted">{label}</dt>
      <dd className="max-w-[65%] text-right text-foreground">{display}</dd>
    </div>
  )
}

function LoadingPlaceholder() {
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      {[1,2,3,4].map(i => (
        <Card key={i} className="h-40 animate-pulse border-border bg-panel" />
      ))}
    </div>
  )
}
