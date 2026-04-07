import { useCallback, useEffect, useMemo, useState } from 'react'
import { getJson } from '../lib/api'

/* ── types ───────────────────────────────────────────────── */

type AuditEntry = {
  id?: string
  timestamp: string
  actor: string
  action: string
  target?: string
  details?: string
  [key: string]: unknown
}

type AuditResponse = {
  events?: AuditEntry[]
  entries?: AuditEntry[]
  items?: AuditEntry[]
  [key: string]: unknown
}

/* ── color mapping ───────────────────────────────────────── */

const ACTION_COLORS: Record<string, { bg: string; text: string }> = {
  config: { bg: 'bg-blue-500/15', text: 'text-blue-300' },
  model: { bg: 'bg-emerald-500/15', text: 'text-emerald-300' },
  node: { bg: 'bg-orange-500/15', text: 'text-orange-300' },
  update: { bg: 'bg-purple-500/15', text: 'text-purple-300' },
  auth: { bg: 'bg-rose-500/15', text: 'text-rose-300' },
  deploy: { bg: 'bg-cyan-500/15', text: 'text-cyan-300' },
}

function actionColor(action: string) {
  const lower = action.toLowerCase()
  for (const [key, colors] of Object.entries(ACTION_COLORS)) {
    if (lower.includes(key)) return colors
  }
  return { bg: 'bg-slate-500/15', text: 'text-slate-300' }
}

function actionTypes(entries: AuditEntry[]): string[] {
  const set = new Set<string>()
  for (const e of entries) {
    const base = e.action.split('.')[0].split('_')[0].split(':')[0].toLowerCase()
    set.add(base)
  }
  return Array.from(set).sort()
}

function fmtTime(iso: string): string {
  try {
    return new Date(iso).toLocaleString()
  } catch {
    return iso
  }
}

/* ── component ───────────────────────────────────────────── */

export function AuditLog() {
  const [entries, setEntries] = useState<AuditEntry[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  /* filters */
  const [filterAction, setFilterAction] = useState('')
  const [filterActor, setFilterActor] = useState('')
  const [dateFrom, setDateFrom] = useState('')
  const [dateTo, setDateTo] = useState('')

  const load = useCallback(async () => {
    try {
      setError(null)
      // try primary, fall back to alternate
      let data: AuditResponse | null = null
      try {
        data = await getJson<AuditResponse>('/api/audit/recent')
      } catch {
        data = await getJson<AuditResponse>('/api/audit/events')
      }
      const list = data?.events ?? data?.entries ?? data?.items ?? (Array.isArray(data) ? (data as AuditEntry[]) : [])
      setEntries(list)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load audit log')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => { load() }, [load])

  const types = useMemo(() => actionTypes(entries), [entries])

  const filtered = useMemo(() => {
    return entries.filter((e) => {
      if (filterAction && !e.action.toLowerCase().includes(filterAction.toLowerCase())) return false
      if (filterActor && !e.actor.toLowerCase().includes(filterActor.toLowerCase())) return false
      if (dateFrom) {
        const d = new Date(e.timestamp)
        if (d < new Date(dateFrom)) return false
      }
      if (dateTo) {
        const d = new Date(e.timestamp)
        const end = new Date(dateTo)
        end.setDate(end.getDate() + 1) // inclusive end
        if (d >= end) return false
      }
      return true
    })
  }, [entries, filterAction, filterActor, dateFrom, dateTo])

  return (
    <section className="space-y-6">
      {/* header */}
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Audit Log</h1>
          <p className="mt-1 text-sm text-slate-400">
            Browse fleet events — filter by action, actor, or date range
          </p>
        </div>
        <button
          onClick={load}
          disabled={loading}
          className="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"
        >
          {loading ? 'Loading…' : '↻ Refresh'}
        </button>
      </div>

      {error && (
        <div className="rounded-xl border border-rose-500/30 bg-rose-500/10 px-4 py-3 text-sm text-rose-200">
          {error}
        </div>
      )}

      {/* filters */}
      <div className="flex flex-wrap gap-3">
        <select
          value={filterAction}
          onChange={(e) => setFilterAction(e.target.value)}
          className="rounded-lg border border-slate-700 bg-slate-900 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
        >
          <option value="">All Actions</option>
          {types.map((t) => (
            <option key={t} value={t}>{t}</option>
          ))}
        </select>

        <input
          type="text"
          placeholder="Filter by actor…"
          value={filterActor}
          onChange={(e) => setFilterActor(e.target.value)}
          className="rounded-lg border border-slate-700 bg-slate-900 px-3 py-2 text-sm text-slate-200 placeholder-slate-500 focus:border-sky-500 focus:outline-none"
        />

        <div className="flex items-center gap-2 text-xs text-slate-400">
          <span>From</span>
          <input
            type="date"
            value={dateFrom}
            onChange={(e) => setDateFrom(e.target.value)}
            className="rounded-lg border border-slate-700 bg-slate-900 px-2 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
          />
          <span>To</span>
          <input
            type="date"
            value={dateTo}
            onChange={(e) => setDateTo(e.target.value)}
            className="rounded-lg border border-slate-700 bg-slate-900 px-2 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
          />
        </div>

        {(filterAction || filterActor || dateFrom || dateTo) && (
          <button
            onClick={() => { setFilterAction(''); setFilterActor(''); setDateFrom(''); setDateTo('') }}
            className="rounded-lg border border-slate-700 px-3 py-2 text-xs text-slate-400 transition hover:bg-slate-800 hover:text-white"
          >
            Clear filters
          </button>
        )}
      </div>

      {/* results count */}
      <p className="text-xs text-slate-500">
        Showing {filtered.length} of {entries.length} event{entries.length !== 1 ? 's' : ''}
      </p>

      {/* table */}
      {loading && entries.length === 0 ? (
        <div className="flex h-48 items-center justify-center text-slate-500">Loading audit log…</div>
      ) : entries.length === 0 ? (
        <div className="flex h-48 items-center justify-center text-slate-500">No audit events found</div>
      ) : (
        <div className="overflow-x-auto rounded-xl border border-slate-800">
          <table className="min-w-full text-sm">
            <thead>
              <tr className="border-b border-slate-800 bg-slate-900/80 text-left text-xs uppercase tracking-wider text-slate-400">
                <th className="px-4 py-3">Timestamp</th>
                <th className="px-4 py-3">Actor</th>
                <th className="px-4 py-3">Action</th>
                <th className="px-4 py-3">Target</th>
                <th className="px-4 py-3">Details</th>
              </tr>
            </thead>
            <tbody>
              {filtered.length === 0 ? (
                <tr>
                  <td colSpan={5} className="px-4 py-8 text-center text-slate-500">
                    No events match current filters
                  </td>
                </tr>
              ) : (
                filtered.map((entry, idx) => {
                  const color = actionColor(entry.action)
                  return (
                    <tr
                      key={entry.id ?? `${entry.timestamp}-${idx}`}
                      className="border-b border-slate-800/50 transition hover:bg-slate-900/40"
                    >
                      <td className="whitespace-nowrap px-4 py-3 text-xs text-slate-400">
                        {fmtTime(entry.timestamp)}
                      </td>
                      <td className="px-4 py-3 font-medium text-slate-200">{entry.actor}</td>
                      <td className="px-4 py-3">
                        <span className={`inline-block rounded-md px-2 py-0.5 text-xs font-medium ${color.bg} ${color.text}`}>
                          {entry.action}
                        </span>
                      </td>
                      <td className="px-4 py-3 text-slate-300">{entry.target ?? '—'}</td>
                      <td className="max-w-xs truncate px-4 py-3 text-xs text-slate-400" title={entry.details ?? ''}>
                        {entry.details ?? '—'}
                      </td>
                    </tr>
                  )
                })
              )}
            </tbody>
          </table>
        </div>
      )}
    </section>
  )
}
