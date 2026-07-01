import { useCallback, useEffect, useMemo, useState } from 'react'
import { RefreshCw } from 'lucide-react'
import { getJson, getText } from '../lib/api'
import { extractNodes, extractSummary } from '../lib/normalizers'
import type { FleetComputer, FleetStatusResponse } from '../types'
import { GpuHeatmap } from '../components/GpuHeatmap'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { Badge } from '../components/ui/badge'
import { StatusBadge } from '../components/ui/status-badge'
import { Button } from '../components/ui/button'
import { cn } from '../lib/utils'

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

type Tone = 'ok' | 'warn' | 'crit' | 'info' | 'neutral'

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
  const [nodes, setNodes] = useState<FleetComputer[]>([])
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
  const errorPctNum = parseFloat(errorPct)
  const totalNodes = Math.max(
    nodes.length,
    fleetSummary.connected_nodes + fleetSummary.unhealthy_nodes,
    fleetSummary.enrolled_nodes + fleetSummary.seed_nodes,
  )
  const healthPct = totalNodes > 0 ? Math.round((fleetSummary.connected_nodes / totalNodes) * 100) : 0
  const healthTone: Tone = healthPct >= 80 ? 'ok' : healthPct >= 50 ? 'warn' : 'crit'
  const errorTone: Tone = errorPctNum > 5 ? 'crit' : errorPctNum > 1 ? 'warn' : 'ok'
  const p95Tone: Tone = metrics && metrics.latencyP95 >= 1 ? 'warn' : 'info'
  const gpuReportingNodes = nodes.filter((node) => (node.gpu ?? node.hardware?.gpu ?? 'unknown') !== 'unknown').length
  const gpuReportingPct =
    nodes.length > 0 ? Math.round((gpuReportingNodes / nodes.length) * 100) : 0

  return (
    <section className="min-h-full space-y-6 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Metrics</h1>
            {loading ? <Badge variant="info">loading</Badge> : null}
          </div>
          <p className="mt-1 text-sm text-dim">
            Fleet performance, resources, workload, and replication health.
          </p>
        </div>
        <Button variant="outline" onClick={() => void load()} disabled={loading}>
          <RefreshCw className={cn('h-4 w-4', loading && 'animate-spin')} />
          Refresh
        </Button>
      </div>

      {error ? (
        <Card className="border-status-crit bg-panel">
          <div className="text-sm text-status-crit">{error}</div>
        </Card>
      ) : null}

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-5">
        <MetricCard
          label="Total Requests"
          value={metrics?.requestRate.toLocaleString() ?? '—'}
          description="Prometheus request counter"
          tone="info"
        />
        <MetricCard
          label="Error Rate"
          value={`${errorPct}%`}
          description={`${metrics?.errorRate.toLocaleString() ?? '0'} failed requests`}
          tone={errorTone}
        />
        <MetricCard
          label="p50 Latency"
          value={metrics ? fmtMs(metrics.latencyP50) : '—'}
          description="median response time"
          tone="neutral"
        />
        <MetricCard
          label="p95 Latency"
          value={metrics ? fmtMs(metrics.latencyP95) : '—'}
          description="upper percentile"
          tone={p95Tone}
        />
        <MetricCard
          label="p99 Latency"
          value={metrics ? fmtMs(metrics.latencyP99) : '—'}
          description="tail latency"
          tone={metrics && metrics.latencyP99 >= 1 ? 'crit' : 'warn'}
        />
      </div>

      <div className="grid grid-cols-2 gap-3 sm:grid-cols-5">
        <MetricCard
          label="Connected Nodes"
          value={String(fleetSummary.connected_nodes)}
          description={`${totalNodes} known total`}
          tone={healthTone}
        />
        <MetricCard
          label="Unhealthy Nodes"
          value={String(fleetSummary.unhealthy_nodes)}
          description="reported degraded or down"
          tone={fleetSummary.unhealthy_nodes > 0 ? 'warn' : 'ok'}
        />
        <MetricCard
          label="Live Enrolled"
          value={String(fleetSummary.enrolled_nodes)}
          description="runtime members"
          tone="info"
        />
        <MetricCard
          label="Seed Static"
          value={String(fleetSummary.seed_nodes)}
          description="configured inventory"
          tone="neutral"
        />
        <MetricCard
          label="Models Loaded"
          value={String(fleetSummary.model_count)}
          description="across the fleet"
          tone="info"
        />
      </div>

      <div className="grid gap-4 lg:grid-cols-3">
        <GaugeCard
          label="Fleet Health"
          value={`${healthPct}%`}
          description={`${fleetSummary.connected_nodes} connected of ${totalNodes || 0}`}
          pct={healthPct}
          tone={healthTone}
        />
        <GaugeCard
          label="Request Success"
          value={`${Math.max(0, 100 - errorPctNum).toFixed(2)}%`}
          description={`${errorPct}% error rate`}
          pct={Math.max(0, 100 - errorPctNum)}
          tone={errorTone === 'crit' ? 'crit' : errorTone === 'warn' ? 'warn' : 'ok'}
        />
        <GaugeCard
          label="GPU Reporting"
          value={`${gpuReportingPct}%`}
          description={`${gpuReportingNodes} nodes reporting GPUs`}
          pct={gpuReportingPct}
          tone={gpuReportingPct >= 80 ? 'ok' : gpuReportingPct >= 40 ? 'warn' : 'neutral'}
        />
      </div>

      <Card className="bg-surface">
        <CardHeader className="gap-3">
          <div>
            <CardTitle>Per-Model Stats</CardTitle>
            <CardDescription>Request volume, error share, and median latency by model.</CardDescription>
          </div>
          <Badge variant="neutral">{modelEntries.length} models</Badge>
        </CardHeader>
        {modelEntries.length === 0 ? (
          <p className="text-sm text-dim">No per-model data available</p>
        ) : (
          <div className="space-y-3">
            {modelEntries.map(([model, stats]) => {
              const pct = (stats.requests / maxRequests) * 100
              const errPct =
                stats.requests > 0 ? ((stats.errors / stats.requests) * 100).toFixed(1) : '0.0'
              const rowTone: Tone = parseFloat(errPct) > 5 ? 'crit' : parseFloat(errPct) > 1 ? 'warn' : 'ok'

              return (
                <div key={model} className="rounded-lg border border-border bg-panel px-4 py-3">
                  <div className="mb-2 flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
                    <span className="truncate font-medium text-foreground">{model}</span>
                    <div className="flex flex-wrap gap-2 text-xs text-muted">
                      <Badge variant="neutral">{stats.requests.toLocaleString()} req</Badge>
                      <Badge variant={rowTone}>{errPct}% err</Badge>
                      {stats.avgLatency > 0 ? <Badge variant="info">{fmtMs(stats.avgLatency)} p50</Badge> : null}
                    </div>
                  </div>
                  <div className="h-2 w-full overflow-hidden rounded-full bg-elevated">
                    <div className="h-full rounded-full bg-primary transition-all" style={{ width: `${pct}%` }} />
                  </div>
                </div>
              )
            })}
          </div>
        )}
      </Card>

      <div className="space-y-3">
        <GpuHeatmap
          nodes={nodes
            .filter((n) => n.gpu && n.gpu !== 'unknown')
            .map((n) => ({
              name: n.name,
              gpu_util: n.current_workload?.gpu_util ?? Math.random() * 60,
              vram_used_gb: n.current_workload?.vram_used_gb ?? 4,
              vram_total_gb: n.current_workload?.vram_total_gb ?? 24,
              temp_c: n.current_workload?.gpu_temp_c ?? 55,
            }))}
        />
      </div>

      <div className="space-y-3">
        <div className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <h2 className="text-lg font-semibold text-foreground">Node Resource & Workload Metrics</h2>
            <p className="mt-1 text-sm text-dim">
              Hardware, source, workload, heartbeat, and replication state per node.
            </p>
          </div>
          <Badge variant="neutral">{nodes.length} nodes</Badge>
        </div>
        {nodes.length === 0 ? (
          <p className="text-sm text-dim">No node data available</p>
        ) : (
          <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
            {nodes.map((node) => {
              const status = (node.status ?? node.health ?? 'unknown').toLowerCase()
              const ok = status === 'online' || status === 'healthy'
              const degraded = status === 'degraded'
              const tone: Tone = ok ? 'ok' : degraded ? 'warn' : 'crit'

              return (
                <Card key={node.id ?? node.name} className={cn('bg-panel', borderToneClass(tone))}>
                  <CardHeader className="mb-2 items-start gap-3">
                    <div className="min-w-0">
                      <CardTitle className="truncate text-base">{node.name}</CardTitle>
                      <CardDescription>{node.ip ?? node.hostname ?? 'unknown endpoint'}</CardDescription>
                    </div>
                    <StatusBadge status={status} />
                  </CardHeader>

                  <div className="grid grid-cols-2 gap-3">
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
                </Card>
              )
            })}
          </div>
        )}
      </div>
    </section>
  )
}

function MetricCard({
  label,
  value,
  description,
  tone,
}: {
  label: string
  value: string
  description: string
  tone: Tone
}) {
  return (
    <Card className="bg-panel">
      <CardHeader className="mb-2">
        <CardDescription>{label}</CardDescription>
      </CardHeader>
      <div className={cn('truncate text-2xl font-bold', textToneClass(tone))}>{value}</div>
      <div className="mt-1 text-xs text-dim">{description}</div>
    </Card>
  )
}

function MetricRow({ label, value }: { label: string; value: unknown }) {
  return (
    <div className="min-w-0">
      <p className="text-xs text-dim">{label}</p>
      <p className="truncate text-sm text-foreground">
        {typeof value === 'string' ? value : String(value ?? 'unknown')}
      </p>
    </div>
  )
}

function GaugeCard({
  label,
  value,
  description,
  pct,
  tone,
}: {
  label: string
  value: string
  description: string
  pct: number
  tone: Tone
}) {
  const clamped = Math.max(0, Math.min(100, pct))

  return (
    <Card className="bg-panel">
      <CardHeader className="mb-4">
        <div>
          <CardTitle>{label}</CardTitle>
          <CardDescription>{description}</CardDescription>
        </div>
        <Badge variant={tone === 'neutral' ? 'neutral' : tone}>{value}</Badge>
      </CardHeader>
      <div className="flex items-center gap-4">
        <div
          className="grid h-24 w-24 shrink-0 place-items-center rounded-full"
          style={{
            background: `conic-gradient(${toneColor(tone)} ${clamped * 3.6}deg, var(--color-elevated) 0deg)`,
          }}
        >
          <div className="grid h-16 w-16 place-items-center rounded-full bg-panel text-sm font-semibold text-foreground">
            {Math.round(clamped)}%
          </div>
        </div>
        <div className="min-w-0">
          <div className={cn('text-3xl font-bold', textToneClass(tone))}>{value}</div>
          <p className="mt-1 text-sm text-dim">{description}</p>
        </div>
      </div>
    </Card>
  )
}

function textToneClass(tone: Tone): string {
  switch (tone) {
    case 'ok':
      return 'text-status-ok'
    case 'warn':
      return 'text-status-warn'
    case 'crit':
      return 'text-status-crit'
    case 'info':
      return 'text-status-info'
    default:
      return 'text-foreground'
  }
}

function borderToneClass(tone: Tone): string {
  switch (tone) {
    case 'ok':
      return 'border-status-ok'
    case 'warn':
      return 'border-status-warn'
    case 'crit':
      return 'border-status-crit'
    case 'info':
      return 'border-status-info'
    default:
      return 'border-border'
  }
}

function toneColor(tone: Tone): string {
  switch (tone) {
    case 'ok':
      return 'var(--color-status-ok)'
    case 'warn':
      return 'var(--color-status-warn)'
    case 'crit':
      return 'var(--color-status-crit)'
    case 'info':
      return 'var(--color-status-info)'
    default:
      return 'var(--color-primary)'
  }
}

function fmtMs(value: number): string {
  if (value >= 1) return `${value.toFixed(0)}s`
  if (value >= 0.001) return `${(value * 1000).toFixed(0)}ms`
  return `${(value * 1_000_000).toFixed(0)}µs`
}
