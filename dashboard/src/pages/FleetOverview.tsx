import { useCallback, useEffect, useMemo, useState, type ElementType } from 'react'
import { Link, useNavigate, useOutletContext } from 'react-router-dom'
import {
  Bot,
  Crown,
  Monitor,
  Network,
  RefreshCw,
  ShieldAlert,
  Wifi,
} from 'lucide-react'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { Badge } from '../components/ui/badge'
import { StatusBadge } from '../components/ui/status-badge'
import { Button } from '../components/ui/button'
import { useFleetStatus } from '../features/hooks/useDashboardQueries'
import { getJson } from '../lib/api'
import { cn } from '../lib/utils'
import type { FleetComputer, FleetStatusSummary, WsEvent } from '../types'

type HealthResponse = { status?: string; [key: string]: unknown }
type Tone = 'ok' | 'warn' | 'crit' | 'info' | 'neutral'

const DEFAULT_SUMMARY: Required<FleetStatusSummary> = {
  total_nodes: 0,
  connected_nodes: 0,
  unhealthy_nodes: 0,
  enrolled_nodes: 0,
  seed_nodes: 0,
  model_count: 0,
  leader: 'unknown',
  gateway_version: 'unknown',
}

export function FleetOverview() {
  const { wsEvent } = useOutletContext<{ wsEvent: WsEvent | null }>()
  const navigate = useNavigate()
  const {
    data: fleet,
    error: fleetError,
    isFetching,
    isLoading,
    refetch,
  } = useFleetStatus()
  const [gatewayHealth, setGatewayHealth] = useState('unknown')
  const [gatewayError, setGatewayError] = useState<string | null>(null)
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null)

  const loadGatewayHealth = useCallback(async () => {
    try {
      setGatewayError(null)
      const healthData = await getJson<HealthResponse>('/health').catch(() => ({ status: 'unknown' }))
      setGatewayHealth(healthData.status ?? 'unknown')
    } catch (err) {
      setGatewayError(err instanceof Error ? err.message : 'Failed to load gateway health')
    }
  }, [])

  const refresh = useCallback(async () => {
    await Promise.all([refetch(), loadGatewayHealth()])
  }, [loadGatewayHealth, refetch])

  useEffect(() => {
    void loadGatewayHealth()
  }, [loadGatewayHealth])

  useEffect(() => {
    const interval = window.setInterval(() => void refresh(), 15000)
    return () => window.clearInterval(interval)
  }, [refresh])

  useEffect(() => {
    if (wsEvent) void refresh()
  }, [refresh, wsEvent])

  useEffect(() => {
    if (fleet) setLastUpdated(new Date())
  }, [fleet])

  const nodes = useMemo(() => {
    return (fleet?.nodes ?? []).filter((node) => {
      const name = (node.name ?? '').toLowerCase()
      return !name.includes('postgres-verify') && !name.includes('verify.local')
    })
  }, [fleet?.nodes])

  const summary = useMemo(() => {
    const source: FleetStatusSummary = fleet?.summary ?? {}
    const connectedFallback = nodes.filter((node) => {
      const status = (node.status ?? node.health ?? '').toLowerCase()
      return status === 'online' || status === 'healthy' || status === 'connected'
    }).length

    return {
      ...DEFAULT_SUMMARY,
      ...source,
      total_nodes: source.total_nodes ?? nodes.length,
      connected_nodes: source.connected_nodes ?? connectedFallback,
      unhealthy_nodes: source.unhealthy_nodes ?? Math.max(nodes.length - connectedFallback, 0),
      model_count:
        source.model_count ??
        nodes.reduce((total, node) => total + (node.models_loaded?.length ?? node.models?.length ?? 0), 0),
    }
  }, [fleet?.summary, nodes])

  const leaderName = useMemo(() => {
    return nodes.find((node) => node.is_leader || node.leader_state === 'leader')?.name ?? summary.leader
  }, [nodes, summary.leader])

  const healthPct =
    summary.total_nodes > 0
      ? Math.round(((summary.total_nodes - summary.unhealthy_nodes) / summary.total_nodes) * 100)
      : 0

  const healthTone: Tone = healthPct >= 80 ? 'ok' : healthPct >= 50 ? 'warn' : 'crit'
  const errorMessage =
    fleetError instanceof Error ? fleetError.message : gatewayError

  return (
    <section className="min-h-full space-y-6 bg-background">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <div className="flex items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Fleet Overview</h1>
            {isFetching && !isLoading ? <Badge variant="info">syncing</Badge> : null}
          </div>
          <p className="mt-1 text-sm text-dim">
            {lastUpdated ? `Updated ${lastUpdated.toLocaleTimeString()}` : 'Loading fleet status'}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant={healthTone}>
            <span className={cn('h-1.5 w-1.5 rounded-full bg-current', statusDotClass(healthTone))} />
            {healthPct}% healthy
          </Badge>
          <StatusBadge status={gatewayHealth}>gateway {gatewayHealth}</StatusBadge>
          <Button variant="outline" onClick={() => void refresh()} disabled={isFetching}>
            <RefreshCw className={cn('h-4 w-4', isFetching && 'animate-spin')} />
            Refresh
          </Button>
        </div>
      </div>

      {isLoading ? (
        <div className="grid gap-4 md:grid-cols-4">
          {[1, 2, 3, 4].map((item) => (
            <MetricSkeleton key={item} />
          ))}
        </div>
      ) : (
        <div className="grid gap-4 md:grid-cols-4">
          <MetricCard
            label="Members Online"
            value={`${summary.connected_nodes}/${summary.total_nodes}`}
            description={`${summary.unhealthy_nodes} unhealthy`}
            tone={healthTone}
            icon={Monitor}
          />
          <MetricCard
            label="Models Loaded"
            value={summary.model_count}
            description="reported by fleet members"
            tone="info"
            icon={Bot}
          />
          <MetricCard
            label="Unhealthy"
            value={summary.unhealthy_nodes}
            description="members needing attention"
            tone={summary.unhealthy_nodes > 0 ? 'crit' : 'ok'}
            icon={ShieldAlert}
          />
          <MetricCard
            label="Leader"
            value={leaderName}
            description="current consensus owner"
            tone="info"
            icon={Crown}
          />
        </div>
      )}

      <div className="grid gap-3 md:grid-cols-4">
        <MiniStat label="Enrolled" value={summary.enrolled_nodes} />
        <MiniStat label="Seed" value={summary.seed_nodes} />
        <MiniStat label="Gateway" value={gatewayHealth} />
        <MiniStat label="Version" value={summary.gateway_version ?? 'v2026.4.7'} />
      </div>

      {errorMessage ? (
        <Card className="border-border bg-panel">
          <div className="flex items-start gap-3 text-sm text-status-crit">
            <ShieldAlert className="mt-0.5 h-4 w-4 flex-shrink-0" />
            <span>{errorMessage}</span>
          </div>
        </Card>
      ) : null}

      <Card className="bg-surface">
        <CardHeader className="gap-3">
          <div>
            <CardTitle>Fleet Nodes</CardTitle>
            <CardDescription>Live members, leader state, workload, and model inventory</CardDescription>
          </div>
          <Badge variant="neutral">{nodes.length} nodes</Badge>
        </CardHeader>

        {isLoading ? (
          <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
            {[1, 2, 3, 4, 5, 6].map((item) => (
              <NodeSkeleton key={item} />
            ))}
          </div>
        ) : nodes.length === 0 ? (
          <EmptyFleetState onPrimary={() => navigate('/onboarding')} />
        ) : (
          <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
            {nodes.map((node) => (
              <FleetNodeCard key={node.id ?? node.name} node={node} />
            ))}
          </div>
        )}
      </Card>
    </section>
  )
}

function MetricCard({
  label,
  value,
  description,
  tone,
  icon: Icon,
}: {
  label: string
  value: string | number
  description: string
  tone: Tone
  icon: ElementType
}) {
  return (
    <Card className="bg-panel">
      <CardHeader className="mb-2">
        <CardDescription className="flex items-center gap-1.5">
          <Icon className="h-3.5 w-3.5" />
          {label}
        </CardDescription>
      </CardHeader>
      <div className={cn('truncate text-2xl font-bold', textToneClass(tone))}>{value}</div>
      <div className="mt-1 text-xs text-dim">{description}</div>
    </Card>
  )
}

function MiniStat({ label, value }: { label: string; value: string | number }) {
  return (
    <div className="flex items-center justify-between rounded-lg border border-border bg-panel px-3 py-2">
      <span className="text-xs text-dim">{label}</span>
      <span className="truncate text-sm font-medium text-muted">{String(value)}</span>
    </div>
  )
}

function FleetNodeCard({ node }: { node: FleetComputer }) {
  const status = (node.status ?? node.health ?? 'unknown').toLowerCase()
  const sourceKind = node.source_kind ?? (node.runtime_enrolled ? 'enrolled/live' : 'seed/static')
  const name = node.name ?? node.id ?? 'Unnamed member'
  const nodeId = encodeURIComponent(node.id ?? node.name)
  const isLeader = node.is_leader || node.leader_state === 'leader'
  const modelsLoaded = node.models_loaded ?? (node.models ?? []).map((model) => model.name)
  const statusTone = toneForNodeStatus(status)

  return (
    <Link to={`/nodes/${nodeId}`} className="group block rounded-xl focus-visible:outline-none">
      <Card className="h-full bg-panel transition group-hover:border-border-subtle group-hover:bg-elevated">
        <CardHeader className="items-start gap-3">
          <div className="min-w-0">
            <CardTitle className="truncate text-base">{name}</CardTitle>
            <CardDescription className="mt-1 flex items-center gap-1.5">
              <Wifi className="h-3.5 w-3.5" />
              <span className="truncate">{node.ip ?? node.hostname ?? 'unknown endpoint'}</span>
            </CardDescription>
          </div>
          <span className={cn('mt-1 h-2.5 w-2.5 flex-shrink-0 rounded-full bg-current', statusDotClass(statusTone))} />
        </CardHeader>

        <div className="flex flex-wrap gap-1.5">
          <StatusBadge status={status} />
          <Badge variant={isLeader ? 'default' : 'neutral'}>{isLeader ? 'leader' : node.leader_state ?? 'follower'}</Badge>
          <Badge variant={sourceKind === 'enrolled/live' ? 'info' : 'neutral'}>{sourceKind}</Badge>
        </div>

        <dl className="mt-4 grid grid-cols-2 gap-3 text-sm">
          <Field label="CPU" value={node.cpu ?? node.hardware?.cpu ?? 'unknown'} />
          <Field label="RAM" value={node.ram ?? node.hardware?.ram ?? 'unknown'} />
          <Field label="GPU" value={node.gpu ?? node.hardware?.gpu ?? 'unknown'} />
          <Field label="Service" value={node.service_version ?? 'unreported'} />
          <Field label="Heartbeat" value={formatHeartbeat(node)} />
          <Field
            label="Replication"
            value={`${node.replication_state?.mode ?? 'unknown'} / ${node.replication_state?.health ?? 'unknown'}`}
          />
          <Field label="Workload" value={formatWorkload(node)} />
          <Field label="Source" value={node.heartbeat_source ?? 'unreported'} />
        </dl>

        {modelsLoaded.length > 0 ? (
          <div className="mt-4 flex flex-wrap gap-1.5">
            {modelsLoaded.map((modelName) => (
              <Badge key={modelName} variant="neutral">
                <Bot className="h-3 w-3" />
                {modelName}
              </Badge>
            ))}
          </div>
        ) : (
          <p className="mt-4 text-xs text-dim">No models reported</p>
        )}
      </Card>
    </Link>
  )
}

function Field({ label, value }: { label: string; value: string | number | undefined }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-dim">{label}</dt>
      <dd className="truncate text-sm text-foreground">{value == null || value === '' ? 'unknown' : String(value)}</dd>
    </div>
  )
}

function EmptyFleetState({ onPrimary }: { onPrimary: () => void }) {
  return (
    <div className="flex flex-col items-center justify-center rounded-xl border border-border bg-panel px-8 py-16 text-center">
      <div className="mb-4 flex h-12 w-12 items-center justify-center rounded-xl bg-primary-subtle text-primary">
        <Network className="h-6 w-6" />
      </div>
      <h3 className="text-lg font-medium text-foreground">No fleet members connected</h3>
      <p className="mt-2 max-w-md text-sm text-dim">
        Add your first fleet member to start monitoring daemon health, leader state, and model inventory.
      </p>
      <div className="mt-6 flex flex-wrap justify-center gap-3">
        <Button onClick={onPrimary}>Add Fleet Member</Button>
        <Button variant="outline" onClick={onPrimary}>
          Setup Guide
        </Button>
      </div>
    </div>
  )
}

function MetricSkeleton() {
  return (
    <Card className="bg-panel">
      <div className="mb-3 h-3 w-24 animate-pulse rounded bg-elevated" />
      <div className="h-8 w-20 animate-pulse rounded bg-elevated" />
      <div className="mt-3 h-3 w-32 animate-pulse rounded bg-elevated" />
    </Card>
  )
}

function NodeSkeleton() {
  return (
    <Card className="space-y-4 bg-panel">
      <div className="flex justify-between gap-3">
        <div className="space-y-2">
          <div className="h-4 w-36 animate-pulse rounded bg-elevated" />
          <div className="h-3 w-28 animate-pulse rounded bg-elevated" />
        </div>
        <div className="h-2.5 w-2.5 animate-pulse rounded-full bg-elevated" />
      </div>
      <div className="grid grid-cols-2 gap-3">
        {[1, 2, 3, 4].map((item) => (
          <div key={item} className="space-y-2">
            <div className="h-3 w-16 animate-pulse rounded bg-elevated" />
            <div className="h-4 w-24 animate-pulse rounded bg-elevated" />
          </div>
        ))}
      </div>
    </Card>
  )
}

function formatWorkload(node: FleetComputer): string {
  const workload = node.current_workload
  if (!workload) return 'unreported'
  if (workload.active_tasks == null) return workload.status
  if (workload.active_tasks === 0) return 'idle'
  return `${workload.active_tasks} active`
}

function formatHeartbeat(node: FleetComputer): string {
  const heartbeat = node.last_heartbeat ?? 'unknown'
  const freshness = node.heartbeat_freshness ?? 'unknown'
  if (!node.heartbeat_age_seconds && freshness === 'unknown') return heartbeat

  const age =
    typeof node.heartbeat_age_seconds === 'number'
      ? `${node.heartbeat_age_seconds}s`
      : 'age unknown'
  return `${heartbeat} (${freshness}, ${age})`
}

function toneForNodeStatus(status: string): Tone {
  if (['online', 'healthy', 'connected', 'active'].includes(status)) return 'ok'
  if (['degraded', 'warning', 'busy'].includes(status)) return 'warn'
  if (['offline', 'failed', 'error', 'critical', 'down'].includes(status)) return 'crit'
  return 'neutral'
}

function textToneClass(tone: Tone) {
  if (tone === 'ok') return 'text-status-ok'
  if (tone === 'warn') return 'text-status-warn'
  if (tone === 'crit') return 'text-status-crit'
  if (tone === 'info') return 'text-status-info'
  return 'text-foreground'
}

function statusDotClass(tone: Tone) {
  if (tone === 'ok') return 'text-status-ok'
  if (tone === 'warn') return 'text-status-warn'
  if (tone === 'crit') return 'text-status-crit'
  if (tone === 'info') return 'text-status-info'
  return 'text-dim'
}
