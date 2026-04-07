import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { getJson } from '../lib/api'
import { extractNodes, extractSummary } from '../lib/normalizers'
import type { FleetNode, FleetStatusResponse } from '../types'

const STATUS_STYLE: Record<string, string> = {
  online: 'border-emerald-500/40 bg-emerald-500/10',
  healthy: 'border-emerald-500/40 bg-emerald-500/10',
  degraded: 'border-amber-500/40 bg-amber-500/10',
  offline: 'border-rose-500/40 bg-rose-500/10',
  unknown: 'border-slate-500/40 bg-slate-500/10',
}

function nodeStatus(node: FleetNode): string {
  return (node.status ?? node.health ?? 'unknown').toLowerCase()
}

function replicationStatus(node: FleetNode): string {
  return node.replication_state?.health ?? 'unknown'
}

function sourceLabel(node: FleetNode): string {
  return node.source_kind ?? (node.runtime_enrolled ? 'enrolled/live' : 'seed/static')
}

export function Topology() {
  const [nodes, setNodes] = useState<FleetNode[]>([])
  const [summary, setSummary] = useState({
    connected_nodes: 0,
    unhealthy_nodes: 0,
    model_count: 0,
    leader: 'unknown',
  })
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const data = await getJson<FleetStatusResponse>('/api/fleet/status')
      const normalizedNodes = extractNodes(data)
      const normalizedSummary = extractSummary(data)

      setNodes(normalizedNodes)
      setSummary({
        connected_nodes: normalizedSummary.connected_nodes ?? 0,
        unhealthy_nodes: normalizedSummary.unhealthy_nodes ?? 0,
        model_count: normalizedSummary.model_count ?? 0,
        leader: normalizedSummary.leader ?? 'unknown',
      })
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load fleet status')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  useEffect(() => {
    const id = window.setInterval(() => void load(), 15000)
    return () => window.clearInterval(id)
  }, [load])

  const leaderNode = useMemo(
    () => nodes.find((node) => node.is_leader || node.leader_state === 'leader') ?? null,
    [nodes],
  )

  const followerNodes = useMemo(
    () => nodes.filter((node) => !(node.is_leader || node.leader_state === 'leader')),
    [nodes],
  )

  return (
    <section className="space-y-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Fleet Topology</h1>
          <p className="mt-1 text-sm text-slate-400">
            Leader/follower topology with replication and health indicators
          </p>
        </div>
        <button
          onClick={() => void load()}
          disabled={loading}
          className="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"
        >
          {loading ? 'Refreshing…' : '↻ Refresh'}
        </button>
      </div>

      {error ? (
        <div className="rounded-xl border border-rose-500/30 bg-rose-500/10 px-4 py-3 text-sm text-rose-200">
          {error}
        </div>
      ) : null}

      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        <StatCard label="Connected Nodes" value={summary.connected_nodes} color="text-emerald-300" />
        <StatCard label="Unhealthy Nodes" value={summary.unhealthy_nodes} color="text-amber-300" />
        <StatCard label="Model Count" value={summary.model_count} color="text-sky-300" />
        <StatCard label="Leader" value={leaderNode?.name ?? summary.leader} />
      </div>

      <div className="space-y-4">
        <h2 className="text-lg font-semibold text-slate-200">Leader</h2>
        {leaderNode ? (
          <TopologyNodeCard node={leaderNode} />
        ) : (
          <p className="text-sm text-slate-400">Leader not yet reported</p>
        )}
      </div>

      <div className="space-y-4">
        <h2 className="text-lg font-semibold text-slate-200">Followers</h2>
        {followerNodes.length === 0 ? (
          <p className="text-sm text-slate-400">No follower nodes reported</p>
        ) : (
          <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
            {followerNodes.map((node) => (
              <TopologyNodeCard key={node.id ?? node.name} node={node} />
            ))}
          </div>
        )}
      </div>

      <div className="rounded-xl border border-slate-800 bg-slate-900/50 px-4 py-3 text-xs text-slate-400">
        Replication health legend: <span className="text-emerald-300">healthy</span>,{' '}
        <span className="text-amber-300">unknown</span>,{' '}
        <span className="text-rose-300">unreported</span>
      </div>
    </section>
  )
}

function TopologyNodeCard({ node }: { node: FleetNode }) {
  const status = nodeStatus(node)
  const style = STATUS_STYLE[status] ?? STATUS_STYLE.unknown
  const replication = replicationStatus(node)
  const replicationColor =
    replication === 'healthy'
      ? 'text-emerald-300'
      : replication === 'unknown'
        ? 'text-amber-300'
        : 'text-rose-300'

  return (
    <article className={`rounded-xl border p-4 ${style}`}>
      <div className="mb-2 flex items-center justify-between">
        <div>
          <h3 className="text-base font-semibold text-slate-100">{node.name}</h3>
          <p className="text-xs text-slate-400">{node.ip ?? 'unknown ip'}</p>
        </div>
        <span className="rounded-full bg-slate-800 px-2 py-1 text-xs text-slate-200">
          {node.is_leader || node.leader_state === 'leader' ? 'leader' : 'follower'}
        </span>
      </div>

      <dl className="grid grid-cols-2 gap-2 text-xs">
        <Row label="Role" value={node.role ?? 'unknown'} />
        <Row label="Status" value={status} />
        <Row label="Source" value={sourceLabel(node)} />
        <Row label="CPU" value={node.cpu ?? node.hardware?.cpu ?? 'unknown'} />
        <Row label="RAM" value={node.ram ?? node.hardware?.ram ?? 'unknown'} />
        <Row label="GPU" value={node.gpu ?? node.hardware?.gpu ?? 'unknown'} />
        <Row label="Heartbeat" value={node.last_heartbeat ?? 'unknown'} />
        <Row
          label="Heartbeat Freshness"
          value={
            node.heartbeat_age_seconds == null
              ? node.heartbeat_freshness ?? 'unknown'
              : `${node.heartbeat_freshness ?? 'unknown'} (${node.heartbeat_age_seconds}s)`
          }
        />
        <Row label="Replication" value={node.replication_state?.mode ?? 'unknown'} />
        <Row label="Workload" value={node.current_workload?.status ?? 'unreported'} />
      </dl>

      <p className="mt-2 text-xs text-slate-400">
        runtime provenance: {(node.runtime_provenance ?? []).join(', ') || 'unreported'}
      </p>
      <p className={`mt-1 text-xs ${replicationColor}`}>
        replication health: {replication} ({node.replication_state?.detail ?? 'unreported'})
      </p>

      <Link
        to={`/nodes/${encodeURIComponent(node.id ?? node.name)}`}
        className="mt-3 inline-block text-xs text-sky-300 hover:text-sky-200"
      >
        View details →
      </Link>
    </article>
  )
}

function StatCard({ label, value, color }: { label: string; value: string | number; color?: string }) {
  return (
    <div className="rounded-lg border border-slate-800 bg-slate-900/50 px-4 py-3">
      <dt className="text-xs text-slate-500">{label}</dt>
      <dd className={`text-xl font-bold ${color ?? 'text-slate-100'}`}>{value}</dd>
    </div>
  )
}

function Row({ label, value }: { label: string; value: unknown }) {
  return (
    <div>
      <dt className="text-slate-500">{label}</dt>
      <dd className="text-slate-200">{typeof value === 'string' ? value : String(value ?? 'unknown')}</dd>
    </div>
  )
}
