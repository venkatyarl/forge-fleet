import { useCallback, useEffect, useMemo, useState } from 'react'
import { getJson, getText } from '../lib/api'
import { extractNodes, extractSummary } from '../lib/normalizers'
import type { FleetNode, FleetStatusResponse } from '../types'

/* ── types ───────────────────────────────────────────────── */

type MetricSample = {
  name: string
  labels: Record<string, string>
  value: number
}

type ParsedMetrics = {
  requestRate: number
  errorRate: number
  latencyP50: number
  latencyP95: number
  latencyP99: number
  perModel: Record<string, { requests: number; errors: number; avgLatency: number }>
}

/* ── prometheus parser ───────────────────────────────────── */

function parsePrometheus(raw: string): MetricSample[] {
  const samples: MetricSample[] = []
  for (const line of raw.split('\n')) {
    const trimmed = line.trim()
    if (!trimmed || trimmed.startsWith('#')) continue

    const match = trimmed.match(
      /^([a-zA-Z_][a-zA-Z0-9_:]*)(\{([^}]*)\})?\s+([\d.eE+-]+|NaN|Inf|\+Inf|-Inf)$/,
    )
    if (!match) continue

    const name = match[1]
    const labelsStr = match[3] ?? ''
    const value = parseFloat(match[4])

    const labels: Record<string, string> = {}
    if (labelsStr) {
      for (const pair of labelsStr.split(',')) {
        const eqIdx = pair.indexOf('=')
        if (eqIdx > 0) {
          const key = pair.slice(0, eqIdx).trim()
          const val = pair.slice(eqIdx + 1).trim().replace(/^"|"$/g, '')
          labels[key] = val
        }
      }
    }
    if (!Number.isNaN(value)) {
      samples.push({ name, labels, value })
    }
  }
  return samples
}

function aggregateMetrics(samples: MetricSample[]): ParsedMetrics {
  let requestRate = 0
  let errorRate = 0
  let latencyP50 = 0
  let latencyP95 = 0
  let latencyP99 = 0
  const perModel: Record<string, { requests: number; errors: number; avgLatency: number }> = {}

  for (const sample of samples) {
    const name = sample.name.toLowerCase()
    const model = sample.labels.model ?? sample.labels.model_name ?? ''

    if (name.includes('request') && name.includes('total') && !name.includes('error')) {
      requestRate += sample.value
      if (model) {
        if (!perModel[model]) perModel[model] = { requests: 0, errors: 0, avgLatency: 0 }
        perModel[model].requests += sample.value
      }
    }

    if (
      (name.includes('error') && name.includes('total')) ||
      (name.includes('request') && sample.labels.status?.startsWith('5'))
    ) {
      errorRate += sample.value
      if (model) {
        if (!perModel[model]) perModel[model] = { requests: 0, errors: 0, avgLatency: 0 }
        perModel[model].errors += sample.value
      }
    }

    if (name.includes('latency') || name.includes('duration')) {
      const quantile = sample.labels.quantile ?? sample.labels.le ?? ''
      if (quantile === '0.5' || quantile === '50') latencyP50 = Math.max(latencyP50, sample.value)
      if (quantile === '0.95' || quantile === '95') latencyP95 = Math.max(latencyP95, sample.value)
      if (quantile === '0.99' || quantile === '99') latencyP99 = Math.max(latencyP99, sample.value)

      if (model) {
        if (!perModel[model]) perModel[model] = { requests: 0, errors: 0, avgLatency: 0 }
        if (quantile === '0.5' || quantile === '50') perModel[model].avgLatency = sample.value
      }
    }
  }

  return { requestRate, errorRate, latencyP50, latencyP95, latencyP99, perModel }
}

/* ── component ───────────────────────────────────────────── */

export function Metrics() {
  const [metrics, setMetrics] = useState<ParsedMetrics | null>(null)
  const [nodes, setNodes] = useState<FleetNode[]>([])
  const [fleetSummary, setFleetSummary] = useState({
    connected_nodes: 0,
    unhealthy_nodes: 0,
    enrolled_nodes: 0,
    seed_nodes: 0,
    model_count: 0,
  })
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const [raw, fleet] = await Promise.all([
        getText('/metrics').catch(() => ''),
        getJson<FleetStatusResponse>('/api/fleet/status').catch(() => ({} as FleetStatusResponse)),
      ])

      if (raw) {
        const samples = parsePrometheus(raw)
        setMetrics(aggregateMetrics(samples))
      }

      setNodes(extractNodes(fleet))
      const summary = extractSummary(fleet)
      setFleetSummary({
        connected_nodes: summary.connected_nodes ?? 0,
        unhealthy_nodes: summary.unhealthy_nodes ?? 0,
        enrolled_nodes: summary.enrolled_nodes ?? 0,
        seed_nodes: summary.seed_nodes ?? 0,
        model_count: summary.model_count ?? 0,
      })
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load metrics')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  useEffect(() => {
    const id = window.setInterval(() => void load(), 30000)
    return () => window.clearInterval(id)
  }, [load])

  const modelEntries = useMemo(
    () => Object.entries(metrics?.perModel ?? {}).sort(([, a], [, b]) => b.requests - a.requests),
    [metrics],
  )

  const maxRequests = useMemo(() => Math.max(1, ...modelEntries.map(([, m]) => m.requests)), [modelEntries])

  const errorPct =
    metrics && metrics.requestRate > 0
      ? ((metrics.errorRate / metrics.requestRate) * 100).toFixed(2)
      : '0.00'

  return (
    <section className="space-y-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Metrics</h1>
          <p className="mt-1 text-sm text-slate-400">
            Fleet performance, resources, workload, and replication health
          </p>
        </div>
        <button
          onClick={() => void load()}
          disabled={loading}
          className="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"
        >
          {loading ? 'Loading…' : '↻ Refresh'}
        </button>
      </div>

      {error ? (
        <div className="rounded-xl border border-rose-500/30 bg-rose-500/10 px-4 py-3 text-sm text-rose-200">
          {error}
        </div>
      ) : null}

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-5">
        <MetricCard label="Total Requests" value={metrics?.requestRate.toLocaleString() ?? '—'} color="text-sky-300" />
        <MetricCard
          label="Error Rate"
          value={`${errorPct}%`}
          color={parseFloat(errorPct) > 5 ? 'text-rose-400' : 'text-emerald-400'}
        />
        <MetricCard label="p50 Latency" value={metrics ? fmtMs(metrics.latencyP50) : '—'} color="text-slate-200" />
        <MetricCard label="p95 Latency" value={metrics ? fmtMs(metrics.latencyP95) : '—'} color="text-amber-300" />
        <MetricCard label="p99 Latency" value={metrics ? fmtMs(metrics.latencyP99) : '—'} color="text-rose-300" />
      </div>

      <div className="grid grid-cols-2 gap-3 sm:grid-cols-5">
        <MetricCard label="Connected Nodes" value={String(fleetSummary.connected_nodes)} color="text-emerald-300" />
        <MetricCard label="Unhealthy Nodes" value={String(fleetSummary.unhealthy_nodes)} color="text-amber-300" />
        <MetricCard label="Live Enrolled" value={String(fleetSummary.enrolled_nodes)} color="text-sky-300" />
        <MetricCard label="Seed Static" value={String(fleetSummary.seed_nodes)} color="text-slate-300" />
        <MetricCard label="Models Loaded" value={String(fleetSummary.model_count)} color="text-sky-300" />
      </div>

      <div className="space-y-3">
        <h2 className="text-lg font-semibold text-slate-200">Per-Model Stats</h2>
        {modelEntries.length === 0 ? (
          <p className="text-sm text-slate-500">No per-model data available</p>
        ) : (
          <div className="space-y-2">
            {modelEntries.map(([model, stats]) => {
              const pct = (stats.requests / maxRequests) * 100
              const errPct =
                stats.requests > 0 ? ((stats.errors / stats.requests) * 100).toFixed(1) : '0.0'

              return (
                <div key={model} className="rounded-lg border border-slate-800 bg-slate-900/50 px-4 py-3">
                  <div className="mb-1 flex items-center justify-between gap-3">
                    <span className="font-medium text-slate-200">{model}</span>
                    <div className="flex gap-4 text-xs text-slate-400">
                      <span>{stats.requests.toLocaleString()} req</span>
                      <span className={parseFloat(errPct) > 5 ? 'text-rose-400' : ''}>{errPct}% err</span>
                      {stats.avgLatency > 0 ? <span>{fmtMs(stats.avgLatency)} p50</span> : null}
                    </div>
                  </div>
                  <div className="h-2 w-full overflow-hidden rounded-full bg-slate-800">
                    <div className="h-full rounded-full bg-sky-500/70 transition-all" style={{ width: `${pct}%` }} />
                  </div>
                </div>
              )
            })}
          </div>
        )}
      </div>

      <div className="space-y-3">
        <h2 className="text-lg font-semibold text-slate-200">Node Resource & Workload Metrics</h2>
        {nodes.length === 0 ? (
          <p className="text-sm text-slate-500">No node data available</p>
        ) : (
          <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
            {nodes.map((node) => {
              const status = (node.status ?? node.health ?? 'unknown').toLowerCase()
              const ok = status === 'online' || status === 'healthy'
              const degraded = status === 'degraded'
              const borderClass = ok
                ? 'border-emerald-500/30 bg-emerald-500/5'
                : degraded
                  ? 'border-amber-500/30 bg-amber-500/5'
                  : 'border-rose-500/30 bg-rose-500/5'

              return (
                <div key={node.id ?? node.name} className={`rounded-lg border px-4 py-3 ${borderClass}`}>
                  <div className="flex items-center justify-between gap-2">
                    <span className="font-medium text-slate-200">{node.name}</span>
                    <span className="text-xs text-slate-300">{status}</span>
                  </div>

                  <div className="mt-2 grid grid-cols-2 gap-1 text-xs text-slate-300">
                    <MetricRow label="CPU" value={node.cpu ?? node.hardware?.cpu ?? 'unknown'} />
                    <MetricRow label="RAM" value={node.ram ?? node.hardware?.ram ?? 'unknown'} />
                    <MetricRow label="GPU" value={node.gpu ?? node.hardware?.gpu ?? 'unknown'} />
                    <MetricRow
                      label="Source"
                      value={node.source_kind ?? (node.runtime_enrolled ? 'enrolled/live' : 'seed/static')}
                    />
                    <MetricRow
                      label="Workload"
                      value={
                        node.current_workload?.active_tasks == null
                          ? node.current_workload?.status ?? 'unreported'
                          : `${node.current_workload.active_tasks} active`
                      }
                    />
                    <MetricRow
                      label="Replication"
                      value={`${node.replication_state?.mode ?? 'unknown'} / ${node.replication_state?.health ?? 'unknown'}`}
                    />
                    <MetricRow
                      label="Heartbeat"
                      value={
                        node.heartbeat_age_seconds == null
                          ? node.last_heartbeat ?? 'unknown'
                          : `${node.last_heartbeat ?? 'unknown'} (${node.heartbeat_freshness ?? 'unknown'}, ${node.heartbeat_age_seconds}s)`
                      }
                    />
                    <MetricRow
                      label="Provenance"
                      value={(node.runtime_provenance ?? []).join(', ') || 'unreported'}
                    />
                  </div>
                </div>
              )
            })}
          </div>
        )}
      </div>
    </section>
  )
}

function MetricCard({ label, value, color }: { label: string; value: string; color?: string }) {
  return (
    <div className="rounded-xl border border-slate-800 bg-slate-900/50 px-4 py-4">
      <p className="text-xs uppercase tracking-wider text-slate-500">{label}</p>
      <p className={`mt-1 text-2xl font-bold ${color ?? 'text-slate-100'}`}>{value}</p>
    </div>
  )
}

function MetricRow({ label, value }: { label: string; value: unknown }) {
  return (
    <div>
      <span className="text-slate-500">{label}: </span>
      <span>{typeof value === 'string' ? value : String(value ?? 'unknown')}</span>
    </div>
  )
}

function fmtMs(value: number): string {
  if (value >= 1) return `${value.toFixed(0)}s`
  if (value >= 0.001) return `${(value * 1000).toFixed(0)}ms`
  return `${(value * 1_000_000).toFixed(0)}µs`
}
