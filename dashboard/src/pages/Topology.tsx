import { useCallback, useEffect, useMemo } from 'react'
import { Link } from 'react-router-dom'
import { GitBranch, RefreshCw } from 'lucide-react'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { Badge } from '../components/ui/badge'
import { StatusBadge } from '../components/ui/status-badge'
import { Button } from '../components/ui/button'
import { useFleetStatus } from '../features/hooks/useDashboardQueries'
import { cn } from '../lib/utils'
import type { FleetComputer, FleetStatusSummary } from '../types'

type Tone = 'ok' | 'warn' | 'crit' | 'info' | 'neutral'

const DEFAULT_SUMMARY: Required<Pick<FleetStatusSummary, 'connected_nodes' | 'unhealthy_nodes' | 'model_count' | 'leader'>> = {
  connected_nodes: 0,
  unhealthy_nodes: 0,
  model_count: 0,
  leader: 'unknown',
}

function nodeStatus(node: FleetComputer): string {
  return (node.status ?? node.health ?? 'unknown').toLowerCase()
}

function replicationStatus(node: FleetComputer): string {
  return node.replication_state?.health ?? 'unreported'
}

function sourceLabel(node: FleetComputer): string {
  return node.source_kind ?? (node.runtime_enrolled ? 'enrolled/live' : 'seed/static')
}

export function Topology() {
  const {
    data: fleet,
    error: fleetError,
    isFetching,
    isLoading,
    refetch,
    dataUpdatedAt,
  } = useFleetStatus()

  const refresh = useCallback(async () => {
    await refetch()
  }, [refetch])

  useEffect(() => {
    const id = window.setInterval(() => void refresh(), 15000)
    return () => window.clearInterval(id)
  }, [refresh])

  const nodes = useMemo(() => fleet?.nodes ?? [], [fleet?.nodes])

  const summary = useMemo(
    () => ({
      ...DEFAULT_SUMMARY,
      ...(fleet?.summary ?? {}),
    }),
    [fleet?.summary],
  )

  const leaderNode = useMemo(
    () => nodes.find((node) => node.is_leader || node.leader_state === 'leader') ?? null,
    [nodes],
  )

  const followerNodes = useMemo(
    () => nodes.filter((node) => !(node.is_leader || node.leader_state === 'leader')),
    [nodes],
  )

  const errorMessage = fleetError instanceof Error ? fleetError.message : null
  const leaderName = leaderNode?.name ?? summary.leader
  const lastUpdated = dataUpdatedAt ? new Date(dataUpdatedAt).toLocaleTimeString() : null

  return (
    <section className="min-h-full space-y-6 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Fleet Topology</h1>
            {isFetching && !isLoading ? <Badge variant="info">syncing</Badge> : null}
          </div>
          <p className="mt-1 text-sm text-dim">
            {lastUpdated
              ? `Updated ${lastUpdated}`
              : 'Leader/follower topology with replication and health indicators'}
          </p>
        </div>

        <Button variant="outline" onClick={() => void refresh()} disabled={isFetching}>
          <RefreshCw className={cn('h-4 w-4', isFetching && 'animate-spin')} />
          Refresh
        </Button>
      </div>

      {errorMessage ? (
        <Card className="border-status-crit bg-panel">
          <div className="text-sm text-status-crit">{errorMessage}</div>
        </Card>
      ) : null}

      {isLoading ? (
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          {[1, 2, 3, 4].map((item) => (
            <StatSkeleton key={item} />
          ))}
        </div>
      ) : (
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          <StatCard label="Connected Nodes" value={summary.connected_nodes} tone="ok" />
          <StatCard label="Unhealthy Nodes" value={summary.unhealthy_nodes} tone={summary.unhealthy_nodes > 0 ? 'warn' : 'ok'} />
          <StatCard label="Model Count" value={summary.model_count} tone="info" />
          <StatCard label="Leader" value={leaderName} tone="neutral" />
        </div>
      )}

      <Card className="bg-surface">
        <CardHeader className="gap-3">
          <div>
            <CardTitle className="flex items-center gap-2 text-base">
              <GitBranch className="h-4 w-4 text-primary" />
              Topology Tree
            </CardTitle>
            <CardDescription>Consensus leader, followers, source, workload, and replication state</CardDescription>
          </div>
          <Badge variant="neutral">{nodes.length} nodes</Badge>
        </CardHeader>

        {isLoading ? (
          <TopologySkeleton />
        ) : (
          <div className="space-y-5">
            <div>
              <SectionLabel label="Leader" count={leaderNode ? 1 : 0} />
              {leaderNode ? (
                <TopologyNodeCard node={leaderNode} emphasis="leader" />
              ) : (
                <EmptyState message="Leader not yet reported" />
              )}
            </div>

            <div className="relative space-y-4 pl-4 sm:pl-6">
              <div className="absolute left-1 top-0 hidden h-full w-px bg-border sm:block" />
              <SectionLabel label="Followers" count={followerNodes.length} />
              {followerNodes.length === 0 ? (
                <EmptyState message="No follower nodes reported" />
              ) : (
                <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
                  {followerNodes.map((node) => (
                    <div key={node.id ?? node.name} className="relative">
                      <div className="absolute -left-5 top-8 hidden h-px w-4 bg-border sm:block" />
                      <TopologyNodeCard node={node} emphasis="follower" />
                    </div>
                  ))}
                </div>
              )}
            </div>
          </div>
        )}
      </Card>

      <Card className="bg-panel">
        <CardHeader className="mb-2">
          <div>
            <CardTitle>Replication Health Legend</CardTitle>
            <CardDescription>Fleet replication states reported by each member</CardDescription>
          </div>
        </CardHeader>
        <div className="flex flex-wrap gap-2">
          <StatusBadge status="healthy">healthy</StatusBadge>
          <StatusBadge status="unknown">unknown</StatusBadge>
          <StatusBadge status="unreported">unreported</StatusBadge>
        </div>
      </Card>
    </section>
  )
}

function TopologyNodeCard({ node, emphasis }: { node: FleetComputer; emphasis: 'leader' | 'follower' }) {
  const status = nodeStatus(node)
  const replication = replicationStatus(node)
  const isLeader = emphasis === 'leader'
  const nodeId = encodeURIComponent(node.id ?? node.name)

  return (
    <Card
      className={cn(
        'h-full bg-panel transition hover:border-border-subtle hover:bg-elevated',
        isLeader && 'border-primary/50 bg-primary-subtle',
      )}
    >
      <CardHeader className="items-start gap-3">
        <div className="min-w-0">
          <CardTitle className="truncate text-base">{node.name}</CardTitle>
          <CardDescription className="mt-1 truncate">{node.ip ?? node.hostname ?? 'unknown endpoint'}</CardDescription>
        </div>
        <span className={cn('mt-1 h-2.5 w-2.5 flex-shrink-0 rounded-full', statusDotClass(toneForStatus(status)))} />
      </CardHeader>

      <div className="flex flex-wrap gap-1.5">
        <StatusBadge status={status} />
        <Badge variant={isLeader ? 'default' : 'neutral'}>{isLeader ? 'leader' : node.leader_state ?? 'follower'}</Badge>
        <Badge variant={sourceLabel(node) === 'enrolled/live' ? 'info' : 'neutral'}>{sourceLabel(node)}</Badge>
      </div>

      <dl className="mt-4 grid grid-cols-2 gap-3 text-sm">
        <Row label="Role" value={node.role ?? 'unknown'} />
        <Row label="CPU" value={node.cpu ?? node.hardware?.cpu ?? 'unknown'} />
        <Row label="RAM" value={node.ram ?? node.hardware?.ram ?? 'unknown'} />
        <Row label="GPU" value={node.gpu ?? node.hardware?.gpu ?? 'unknown'} />
        <Row label="Heartbeat" value={node.last_heartbeat ?? 'unknown'} />
        <Row label="Freshness" value={formatHeartbeatFreshness(node)} />
        <Row label="Replication" value={node.replication_state?.mode ?? 'unknown'} />
        <Row label="Workload" value={formatWorkload(node)} />
      </dl>

      <div className="mt-4 space-y-2 border-t border-border pt-3">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs text-dim">replication health</span>
          <StatusBadge status={replication}>{replication}</StatusBadge>
          <span className={cn('text-xs', textToneClass(toneForStatus(replication)))}>
            {node.replication_state?.detail ?? 'unreported'}
          </span>
        </div>
        <p className="text-xs text-dim">
          runtime provenance:{' '}
          <span className="text-muted">{(node.runtime_provenance ?? []).join(', ') || 'unreported'}</span>
        </p>
      </div>

      <Link
        to={`/nodes/${nodeId}`}
        className="mt-4 inline-flex text-xs font-medium text-primary transition hover:text-primary-muted"
      >
        View details
      </Link>
    </Card>
  )
}

function StatCard({ label, value, tone }: { label: string; value: string | number; tone: Tone }) {
  return (
    <Card className="bg-panel">
      <dt className="text-xs text-dim">{label}</dt>
      <dd className={cn('mt-1 truncate text-xl font-bold', textToneClass(tone))}>{value}</dd>
    </Card>
  )
}

function Row({ label, value }: { label: string; value: unknown }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-dim">{label}</dt>
      <dd className="truncate text-sm text-muted">{typeof value === 'string' ? value : String(value ?? 'unknown')}</dd>
    </div>
  )
}

function SectionLabel({ label, count }: { label: string; count: number }) {
  return (
    <div className="mb-3 flex items-center gap-2">
      <h2 className="text-sm font-semibold text-foreground">{label}</h2>
      <Badge variant="neutral">{count}</Badge>
    </div>
  )
}

function EmptyState({ message }: { message: string }) {
  return (
    <div className="rounded-lg border border-border bg-panel px-4 py-6 text-sm text-dim">
      {message}
    </div>
  )
}

function StatSkeleton() {
  return (
    <Card className="bg-panel">
      <div className="h-3 w-24 animate-pulse rounded bg-elevated" />
      <div className="mt-3 h-6 w-16 animate-pulse rounded bg-elevated" />
    </Card>
  )
}

function TopologySkeleton() {
  return (
    <div className="space-y-4">
      {[1, 2, 3].map((item) => (
        <Card key={item} className="space-y-4 bg-panel">
          <div className="flex items-start justify-between gap-3">
            <div className="space-y-2">
              <div className="h-4 w-36 animate-pulse rounded bg-elevated" />
              <div className="h-3 w-28 animate-pulse rounded bg-elevated" />
            </div>
            <div className="h-2.5 w-2.5 animate-pulse rounded-full bg-elevated" />
          </div>
          <div className="grid grid-cols-2 gap-3">
            {[1, 2, 3, 4].map((field) => (
              <div key={field} className="space-y-2">
                <div className="h-3 w-16 animate-pulse rounded bg-elevated" />
                <div className="h-4 w-24 animate-pulse rounded bg-elevated" />
              </div>
            ))}
          </div>
        </Card>
      ))}
    </div>
  )
}

function formatHeartbeatFreshness(node: FleetComputer): string {
  if (node.heartbeat_age_seconds == null) return node.heartbeat_freshness ?? 'unknown'
  return `${node.heartbeat_freshness ?? 'unknown'} (${node.heartbeat_age_seconds}s)`
}

function formatWorkload(node: FleetComputer): string {
  const workload = node.current_workload
  if (!workload) return 'unreported'
  if (workload.active_tasks == null) return workload.status
  if (workload.active_tasks === 0) return 'idle'
  return `${workload.active_tasks} active`
}

function toneForStatus(status: string): Tone {
  const normalized = status.toLowerCase()
  if (['online', 'healthy', 'connected', 'active', 'ready'].includes(normalized)) return 'ok'
  if (['degraded', 'warning', 'busy', 'unknown'].includes(normalized)) return 'warn'
  if (['offline', 'failed', 'error', 'critical', 'down', 'unreported'].includes(normalized)) return 'crit'
  if (['pending', 'queued', 'standby', 'info'].includes(normalized)) return 'info'
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
  if (tone === 'ok') return 'bg-status-ok'
  if (tone === 'warn') return 'bg-status-warn'
  if (tone === 'crit') return 'bg-status-crit'
  if (tone === 'info') return 'bg-status-info'
  return 'bg-border-subtle'
}
