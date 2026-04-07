import { useCallback, useEffect, useState } from 'react'
import { useOutletContext } from 'react-router-dom'
import { getJson } from '../lib/api'
import type { WsEvent } from '../types'

type ProxyStats = {
  totalRequests?: number
  avgLatencyMs?: number
  errorRate?: number
  activeRoutes?: number
  [key: string]: unknown
}

type ProxyRequest = {
  id?: string
  model?: string
  tier?: number
  latencyMs?: number
  decision?: string
  status?: string
  timestamp?: string
}

export function LLMProxy() {
  const { wsEvent } = useOutletContext<{ wsEvent: WsEvent | null }>()
  const [stats, setStats] = useState<ProxyStats>({})
  const [requests, setRequests] = useState<ProxyRequest[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const [statsPayload, requestsPayload] = await Promise.all([
        getJson<ProxyStats>('/api/proxy/stats').catch(() => getJson<ProxyStats>('/v1/proxy/stats')),
        getJson<{ requests?: ProxyRequest[] }>('/api/proxy/requests').catch(() =>
          getJson<{ requests?: ProxyRequest[] }>('/v1/proxy/requests'),
        ),
      ])
      setStats(statsPayload)
      setRequests(requestsPayload.requests ?? [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load proxy stats')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
    const interval = window.setInterval(() => void load(), 10000)
    return () => window.clearInterval(interval)
  }, [load])

  useEffect(() => {
    if (wsEvent?.type.toLowerCase().includes('proxy')) {
      void load()
    }
  }, [wsEvent, load])

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-xl font-semibold text-slate-100">LLM Proxy</h2>
        <button
          onClick={() => void load()}
          className="rounded-md border border-slate-700 bg-slate-900 px-3 py-1.5 text-sm text-slate-200 hover:border-slate-500"
          type="button"
        >
          Refresh
        </button>
      </div>

      <div className="grid gap-3 md:grid-cols-4">
        <Stat label="Total Requests" value={String(stats.totalRequests ?? 0)} />
        <Stat label="Avg Latency" value={`${stats.avgLatencyMs ?? 0} ms`} />
        <Stat label="Error Rate" value={`${stats.errorRate ?? 0}%`} />
        <Stat label="Active Routes" value={String(stats.activeRoutes ?? 0)} />
      </div>

      {loading ? <Info text="Loading proxy data..." /> : null}
      {error ? <Info text={`Error: ${error}`} danger /> : null}

      <div className="overflow-hidden rounded-xl border border-slate-800 bg-slate-900/70">
        <table className="min-w-full text-left text-sm">
          <thead className="bg-slate-900 text-slate-400">
            <tr>
              <th className="px-3 py-2">Time</th>
              <th className="px-3 py-2">Model</th>
              <th className="px-3 py-2">Tier</th>
              <th className="px-3 py-2">Decision</th>
              <th className="px-3 py-2">Latency</th>
              <th className="px-3 py-2">Status</th>
            </tr>
          </thead>
          <tbody>
            {requests.map((request, idx) => (
              <tr key={`${request.id ?? idx}`} className="border-t border-slate-800 text-slate-200">
                <td className="px-3 py-2">
                  {request.timestamp ? new Date(request.timestamp).toLocaleTimeString() : '-'}
                </td>
                <td className="px-3 py-2">{request.model ?? '-'}</td>
                <td className="px-3 py-2">{request.tier ?? '-'}</td>
                <td className="px-3 py-2">{request.decision ?? '-'}</td>
                <td className="px-3 py-2">{request.latencyMs ?? '-'} ms</td>
                <td className="px-3 py-2">{request.status ?? '-'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  )
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <article className="rounded-xl border border-slate-800 bg-slate-900/70 p-4">
      <p className="text-xs uppercase tracking-wide text-slate-500">{label}</p>
      <p className="mt-2 text-xl font-semibold text-slate-100">{value}</p>
    </article>
  )
}

function Info({ text, danger = false }: { text: string; danger?: boolean }) {
  return (
    <div
      className={`rounded-xl border px-4 py-3 text-sm ${
        danger
          ? 'border-rose-500/30 bg-rose-500/10 text-rose-200'
          : 'border-slate-800 bg-slate-900/50 text-slate-300'
      }`}
    >
      {text}
    </div>
  )
}
