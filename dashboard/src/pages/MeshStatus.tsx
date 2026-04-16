import { useEffect, useMemo, useState } from 'react'
import { getJson } from '../lib/api'

interface MeshRow {
  src_node: string
  dst_node: string
  status: string
  last_checked: string | null
  last_error: string | null
  attempts: number
}

interface MeshResp {
  matrix: MeshRow[]
  count: number
}

interface RetryTask {
  id: string
  title: string
  status: string
  attempts: number
  max_attempts: number
  last_error?: string | null
}

function cellColor(status: string | undefined): string {
  switch (status) {
    case 'ok':
      return 'text-emerald-400'
    case 'failed':
      return 'text-rose-400'
    case 'pending':
      return 'text-amber-400'
    default:
      return 'text-slate-500'
  }
}

function cellIcon(status: string | undefined): string {
  switch (status) {
    case 'ok':
      return '✓'
    case 'failed':
      return '✗'
    case 'pending':
      return '…'
    default:
      return '—'
  }
}

export function MeshStatus() {
  const [rows, setRows] = useState<MeshRow[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [selected, setSelected] = useState<{ src: string; dst: string } | null>(null)
  const [retryTask, setRetryTask] = useState<RetryTask | null>(null)
  const [retrying, setRetrying] = useState(false)

  useEffect(() => {
    let cancelled = false
    async function load() {
      try {
        const d = await getJson<MeshResp>('/api/fleet/mesh-check')
        if (cancelled) return
        setRows(d.matrix || [])
        setLoading(false)
      } catch (e) {
        if (cancelled) return
        setError(e instanceof Error ? e.message : String(e))
        setLoading(false)
      }
    }
    load()
    const t = setInterval(load, 30000)
    return () => {
      cancelled = true
      clearInterval(t)
    }
  }, [])

  const nodes = useMemo(() => {
    const s = new Set<string>()
    for (const r of rows) {
      s.add(r.src_node)
      s.add(r.dst_node)
    }
    return Array.from(s).sort()
  }, [rows])

  const lookup = useMemo(() => {
    const m = new Map<string, MeshRow>()
    for (const r of rows) m.set(`${r.src_node}→${r.dst_node}`, r)
    return m
  }, [rows])

  const selRow = selected && lookup.get(`${selected.src}→${selected.dst}`)

  useEffect(() => {
    if (!selected || !selRow || selRow.status === 'ok') {
      setRetryTask(null)
      return
    }
    const qs = new URLSearchParams({ status: 'pending', kind: 'mesh_retry' })
    getJson<{ tasks: (RetryTask & { payload: { src?: string; dst?: string } })[] }>(
      `/api/fleet/deferred?${qs.toString()}`
    )
      .then((r) => {
        const match = r.tasks.find(
          (t) => t.payload?.src === selected.src && t.payload?.dst === selected.dst
        )
        setRetryTask(match || null)
      })
      .catch(() => setRetryTask(null))
  }, [selected, selRow])

  const runRetry = async () => {
    if (!retryTask) return
    setRetrying(true)
    try {
      await fetch(`/api/fleet/deferred/${retryTask.id}/promote`, { method: 'POST' })
      setRetryTask(null)
    } finally {
      setRetrying(false)
    }
  }

  if (loading) return <div className="p-6 text-slate-400">Loading mesh matrix…</div>
  if (error) return <div className="p-6 text-rose-400">Error: {error}</div>
  if (nodes.length === 0) {
    return (
      <div className="p-6 text-slate-400">
        No mesh status rows yet. Run{' '}
        <code className="bg-slate-800 px-1 rounded">ff fleet ssh-mesh-check</code> on taylor to
        populate.
      </div>
    )
  }

  return (
    <div className="p-6 text-slate-100">
      <h2 className="text-xl font-semibold mb-1">Mesh SSH reachability</h2>
      <p className="text-sm text-slate-400 mb-4">
        Rows = src · Cols = dst · click any cell for detail. ✓ ok &nbsp; ✗ failed &nbsp; … pending
      </p>

      <div className="overflow-x-auto">
        <table className="border-collapse text-sm">
          <thead>
            <tr>
              <th className="text-left px-3 py-2 border-b border-slate-700">src \\ dst</th>
              {nodes.map((n) => (
                <th key={n} className="text-left px-3 py-2 border-b border-slate-700">
                  {n}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {nodes.map((src) => (
              <tr key={src} className="hover:bg-slate-900/50">
                <td className="px-3 py-1.5 font-mono text-slate-300 border-b border-slate-800">
                  {src}
                </td>
                {nodes.map((dst) => {
                  if (src === dst) {
                    return (
                      <td
                        key={dst}
                        className="px-3 py-1.5 border-b border-slate-800 text-slate-700"
                      >
                        ·
                      </td>
                    )
                  }
                  const row = lookup.get(`${src}→${dst}`)
                  return (
                    <td
                      key={dst}
                      onClick={() => setSelected({ src, dst })}
                      className="px-3 py-1.5 border-b border-slate-800 cursor-pointer font-mono text-xs text-center"
                    >
                      <span className={cellColor(row?.status)}>{cellIcon(row?.status)}</span>
                    </td>
                  )
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {selected && selRow && (
        <div className="mt-6 border border-slate-700 rounded p-4 bg-slate-900/60">
          <div className="text-[11px] uppercase tracking-wider text-slate-400 mb-1">
            {selected.src} → {selected.dst}
          </div>
          <div className="grid grid-cols-2 gap-x-6 gap-y-1 text-sm font-mono">
            <div className="text-slate-400">status</div>
            <div className={cellColor(selRow.status)}>{selRow.status}</div>
            <div className="text-slate-400">last_checked</div>
            <div className="text-slate-300">{selRow.last_checked || '—'}</div>
            <div className="text-slate-400">attempts</div>
            <div className="text-slate-300">{selRow.attempts}</div>
            <div className="text-slate-400">last_error</div>
            <div className="text-rose-300 break-all">{selRow.last_error || '—'}</div>
          </div>
          {retryTask && (
            <div className="mt-4 pt-3 border-t border-slate-700">
              <div className="text-[11px] uppercase tracking-wider text-amber-300 mb-1">
                Pending retry (attempts: {retryTask.attempts}/{retryTask.max_attempts})
              </div>
              <button
                onClick={runRetry}
                disabled={retrying}
                className="text-xs px-3 py-1.5 rounded border border-emerald-400 text-emerald-200 hover:bg-emerald-500/20 disabled:opacity-50"
              >
                {retrying ? 'Promoting…' : 'Run retry now'}
              </button>
            </div>
          )}
          <button
            onClick={() => setSelected(null)}
            className="mt-3 text-xs px-2 py-1 border border-slate-700 rounded text-slate-300 hover:bg-slate-800"
          >
            Close
          </button>
        </div>
      )}
    </div>
  )
}
