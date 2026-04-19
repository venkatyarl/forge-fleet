import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'

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

function statusDot(status: string): string {
  switch (status) {
    case 'online':
      return 'bg-emerald-400'
    case 'sdown':
    case 'maintenance':
      return 'bg-amber-400'
    case 'odown':
    case 'offline':
      return 'bg-rose-500'
    case 'pending':
      return 'bg-sky-400'
    default:
      return 'bg-zinc-500'
  }
}

function statusBadge(status: string): string {
  switch (status) {
    case 'online':
      return 'bg-emerald-500/15 text-emerald-300 border-emerald-500/30'
    case 'sdown':
    case 'maintenance':
      return 'bg-amber-500/15 text-amber-300 border-amber-500/30'
    case 'odown':
    case 'offline':
      return 'bg-rose-500/15 text-rose-300 border-rose-500/30'
    case 'pending':
      return 'bg-sky-500/15 text-sky-300 border-sky-500/30'
    default:
      return 'bg-zinc-800 text-zinc-300 border-zinc-700'
  }
}

function roleBadge(role?: string | null): string {
  if (role === 'leader') return 'bg-violet-500/15 text-violet-300 border-violet-500/30'
  if (role === 'member') return 'bg-zinc-800 text-zinc-300 border-zinc-700'
  return 'bg-zinc-800 text-zinc-500 border-zinc-700'
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

  useEffect(() => {
    void load()
    const i = setInterval(() => void load(), 10_000)
    return () => clearInterval(i)
  }, [load])

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-semibold text-zinc-100">Fleet Overview</h2>
          <p className="text-sm text-zinc-500">
            {rows.length} computer{rows.length === 1 ? '' : 's'} enrolled
          </p>
        </div>
        <button
          onClick={() => void load()}
          className="rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-1.5 text-sm text-zinc-400 hover:text-zinc-200"
        >
          Refresh
        </button>
      </div>

      {error && (
        <div className="rounded-xl border border-rose-500/20 bg-rose-500/5 px-4 py-3 text-sm text-rose-300">
          {error}
        </div>
      )}

      {loading ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {rows.map((c) => (
            <article
              key={c.id}
              className="rounded-xl border border-zinc-800 bg-zinc-900/70 p-4 shadow-sm"
            >
              <div className="mb-3 flex items-center justify-between">
                <div className="flex items-center gap-2">
                  <span className={`h-2.5 w-2.5 rounded-full ${statusDot(c.status)}`} />
                  <div>
                    <h3 className="text-base font-semibold text-zinc-100">{c.name}</h3>
                    <p className="text-xs text-zinc-500">
                      {c.primary_ip}
                      {c.os_distribution ? ` • ${c.os_distribution}` : ''}
                    </p>
                  </div>
                </div>
                <div className="flex items-center gap-1">
                  <span
                    className={`rounded-full border px-2 py-0.5 text-[11px] ${statusBadge(c.status)}`}
                  >
                    {c.status}
                  </span>
                  {c.member_role && (
                    <span
                      className={`rounded-full border px-2 py-0.5 text-[11px] ${roleBadge(c.member_role)}`}
                    >
                      {c.member_role}
                    </span>
                  )}
                </div>
              </div>

              <div className="space-y-2">
                <Bar label="CPU" pct={c.latest_cpu_pct} />
                <Bar label="RAM" pct={c.latest_ram_pct} />
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
          ))}
        </div>
      )}
    </section>
  )
}
