import { useCallback, useEffect, useRef, useState } from 'react'
import { getJson } from '../lib/api'
import { useFleetEvents } from '../lib/useFleetEvents'
import { LiveIndicator, PanelHeader, RefreshButton } from './PanelHeader'
import { Sparkline } from './Sparkline'
import { StatusBadge, StatusDot, toneFor } from './StatusBadge'

// Pulse v2 fleet computer as returned by /api/fleet/computers.
type PulseComputer = {
  id: string
  name: string
  primary_ip: string
  hostname?: string | null
  os_family: string
  os_distribution?: string | null
  os_version?: string | null
  cpu_cores?: number | null
  total_ram_gb?: number | null
  total_disk_gb?: number | null
  has_gpu: boolean
  gpu_kind?: string | null
  gpu_count?: number | null
  gpu_model?: string | null
  gpu_total_vram_gb?: number | null
  status: string
  enrolled_at?: string | null
  last_seen_at?: string | null
  offline_since?: string | null
  member_role?: string | null
  member_runtime?: string | null
  election_priority?: number | null
  active_deployment_count: number
  latest_cpu_pct?: number | null
  latest_ram_pct?: number | null
  latest_disk_free_gb?: number | null
  latest_recorded_at?: string | null
}

type MetricPoint = {
  recorded_at: string
  cpu_pct?: number | null
  ram_pct?: number | null
}

function Bar({ pct, label }: { pct?: number | null; label: string }) {
  const v = typeof pct === 'number' ? Math.min(100, Math.max(0, pct)) : null
  const color =
    v == null
      ? 'bg-zinc-700'
      : v > 90
        ? 'bg-rose-500'
        : v > 70
          ? 'bg-amber-400'
          : 'bg-emerald-400'
  return (
    <div>
      <div className="flex items-center justify-between text-[11px] text-zinc-500">
        <span>{label}</span>
        <span>{v == null ? '—' : `${v.toFixed(0)}%`}</span>
      </div>
      <div className="mt-1 h-1.5 w-full overflow-hidden rounded-full bg-zinc-800">
        <div className={`h-full ${color}`} style={{ width: `${v ?? 0}%` }} />
      </div>
    </div>
  )
}

export function FleetOverviewPanel() {
  const [rows, setRows] = useState<PulseComputer[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  // computer.name → CPU% history series (last ~hour, ~60 points).
  const [history, setHistory] = useState<Record<string, number[]>>({})
  // Deduplicate in-flight history fetches per computer.
  const historyInflight = useRef<Set<string>>(new Set())

  const load = useCallback(async () => {
    try {
      setError(null)
      const data = await getJson<{ computers: PulseComputer[] }>('/api/fleet/computers')
      setRows(data.computers ?? [])
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setLoading(false)
    }
  }, [])

  const loadHistoryFor = useCallback(async (name: string) => {
    if (historyInflight.current.has(name)) return
    historyInflight.current.add(name)
    try {
      const data = await getJson<{ points: MetricPoint[] }>(
        `/api/metrics/${encodeURIComponent(name)}/history?hours=1`,
      )
      const series = (data.points ?? [])
        .map((p) => (typeof p.cpu_pct === 'number' ? p.cpu_pct : null))
        .filter((v): v is number => v !== null)
      setHistory((h) => ({ ...h, [name]: series }))
    } catch {
      // Leave existing series in place; the sparkline will show the
      // "not enough data" dashed line if we have <2 points.
    } finally {
      historyInflight.current.delete(name)
    }
  }, [])

  useEffect(() => {
    void load()
    const i = setInterval(() => void load(), 10_000)
    return () => clearInterval(i)
  }, [load])

  // When the list of computers changes, fetch history for any we don't
  // have yet. Also refresh history every 5 minutes so the sparkline
  // stays current.
  useEffect(() => {
    for (const c of rows) {
      if (history[c.name] === undefined) void loadHistoryFor(c.name)
    }
    const i = setInterval(() => {
      for (const c of rows) void loadHistoryFor(c.name)
    }, 300_000)
    return () => clearInterval(i)
  }, [rows, history, loadHistoryFor])

  // Real-time fleet events: refresh computer list on member transitions.
  const { live } = useFleetEvents((evt) => {
    if (evt.subject.startsWith('fleet.events.member.')) {
      void load()
    }
  })

  return (
    <section className="space-y-4">
      <PanelHeader
        title="Fleet Overview"
        subtitle={`${rows.length} computer${rows.length === 1 ? '' : 's'} enrolled`}
        rightSlot={
          <>
            <LiveIndicator live={live} />
            <RefreshButton onClick={() => void load()} />
          </>
        }
      />

      {error && (
        <div className="rounded-xl border border-rose-500/20 bg-rose-500/5 px-4 py-3 text-sm text-rose-300">
          {error}
        </div>
      )}

      {loading ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {rows.map((c) => {
            const series = history[c.name] ?? []
            return (
              <article
                key={c.id}
                className="rounded-xl border border-zinc-800 bg-zinc-900/50 p-4 shadow-sm"
              >
                <div className="mb-3 flex items-center justify-between">
                  <div className="flex items-center gap-2">
                    <StatusDot status={c.status} />
                    <div>
                      <h3 className="text-base font-semibold text-zinc-100">{c.name}</h3>
                      <p className="text-xs text-zinc-500">
                        {c.primary_ip}
                        {c.os_distribution ? ` • ${c.os_distribution}` : ''}
                      </p>
                    </div>
                  </div>
                  <div className="flex items-center gap-1">
                    <StatusBadge status={c.status} />
                    {c.member_role && (
                      <StatusBadge
                        tone={toneFor(c.member_role)}
                        status={c.member_role}
                      />
                    )}
                  </div>
                </div>

                <div className="space-y-2">
                  <Bar label="CPU" pct={c.latest_cpu_pct} />
                  <Bar label="RAM" pct={c.latest_ram_pct} />
                </div>

                <div className="mt-3 flex items-center justify-between gap-2 rounded-lg border border-zinc-800/80 bg-zinc-950/40 px-2 py-1.5">
                  <div className="text-[10px] uppercase tracking-wider text-zinc-500">
                    CPU · 1h
                  </div>
                  <Sparkline
                    data={series}
                    width={100}
                    height={20}
                    yMin={0}
                    yMax={100}
                    title={`CPU% over the last hour on ${c.name}`}
                  />
                </div>

                <dl className="mt-3 grid grid-cols-2 gap-2 text-xs">
                  <div>
                    <dt className="text-zinc-500">Cores / RAM</dt>
                    <dd className="text-zinc-300">
                      {c.cpu_cores ?? '—'} / {c.total_ram_gb ?? '—'}G
                    </dd>
                  </div>
                  <div>
                    <dt className="text-zinc-500">GPU</dt>
                    <dd className="text-zinc-300">
                      {c.has_gpu
                        ? `${c.gpu_kind ?? 'gpu'}${c.gpu_total_vram_gb ? ` ${c.gpu_total_vram_gb.toFixed(0)}G` : ''}`
                        : 'none'}
                    </dd>
                  </div>
                  <div>
                    <dt className="text-zinc-500">LLM servers</dt>
                    <dd className="text-zinc-300">{c.active_deployment_count}</dd>
                  </div>
                  <div>
                    <dt className="text-zinc-500">Disk free</dt>
                    <dd className="text-zinc-300">
                      {c.latest_disk_free_gb == null
                        ? '—'
                        : `${c.latest_disk_free_gb.toFixed(0)}G`}
                    </dd>
                  </div>
                </dl>
              </article>
            )
          })}
        </div>
      )}
    </section>
  )
}
