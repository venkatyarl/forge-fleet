import { useCallback, useEffect, useState } from 'react'
import { useOutletContext } from 'react-router-dom'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { getJson } from '../lib/api'
import { cn } from '../lib/utils'
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
    <section className="space-y-5">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <h2 className="text-xl font-semibold text-foreground">LLM Proxy</h2>
          <p className="mt-1 text-sm text-muted">Routing health, latency, and recent proxy decisions.</p>
        </div>
        <div className="flex items-center gap-2">
          {loading ? <Badge variant="info">loading</Badge> : <Badge variant="ok">live</Badge>}
          <Button onClick={() => void load()} type="button" variant="outline" disabled={loading}>
            Refresh
          </Button>
        </div>
      </div>

      <div className="grid gap-3 md:grid-cols-4">
        <Stat label="Total Requests" value={String(stats.totalRequests ?? 0)} />
        <Stat label="Avg Latency" value={`${stats.avgLatencyMs ?? 0} ms`} />
        <Stat label="Error Rate" value={`${stats.errorRate ?? 0}%`} />
        <Stat label="Active Routes" value={String(stats.activeRoutes ?? 0)} />
      </div>

      {loading ? <Info text="Loading proxy data..." /> : null}
      {error ? <Info text={`Error: ${error}`} danger /> : null}

      <Card className="overflow-hidden bg-surface p-0">
        <CardHeader className="mb-0 border-b border-border px-4 py-3">
          <div>
            <CardTitle>Recent Requests</CardTitle>
            <CardDescription>Latest proxy traffic from the request log.</CardDescription>
          </div>
          <Badge variant="neutral">{requests.length} rows</Badge>
        </CardHeader>
        <div className="overflow-x-auto">
          <table className="min-w-full text-left text-sm">
            <thead className="border-b border-border bg-elevated text-xs uppercase text-dim">
              <tr>
                <th className="px-4 py-2 font-medium">Time</th>
                <th className="px-4 py-2 font-medium">Model</th>
                <th className="px-4 py-2 font-medium">Tier</th>
                <th className="px-4 py-2 font-medium">Decision</th>
                <th className="px-4 py-2 font-medium">Latency</th>
                <th className="px-4 py-2 font-medium">Status</th>
              </tr>
            </thead>
            <tbody>
              {requests.length === 0 ? (
                <tr>
                  <td className="px-4 py-8 text-center text-sm text-dim" colSpan={6}>
                    No proxy requests reported yet.
                  </td>
                </tr>
              ) : (
                requests.map((request, idx) => (
                  <tr
                    key={`${request.id ?? idx}`}
                    className="border-t border-border text-muted transition hover:bg-panel hover:text-foreground"
                  >
                    <td className="whitespace-nowrap px-4 py-3 text-dim">
                      {request.timestamp ? new Date(request.timestamp).toLocaleTimeString() : '-'}
                    </td>
                    <td className="px-4 py-3 font-mono text-xs text-status-info">{request.model ?? '-'}</td>
                    <td className="px-4 py-3">
                      {request.tier == null ? '-' : <Badge variant="neutral">tier {request.tier}</Badge>}
                    </td>
                    <td className="px-4 py-3">
                      {request.decision ? <Badge variant="default">{request.decision}</Badge> : '-'}
                    </td>
                    <td className="whitespace-nowrap px-4 py-3 text-foreground">
                      {request.latencyMs ?? '-'} ms
                    </td>
                    <td className="px-4 py-3">
                      <StatusBadge status={request.status ?? 'unknown'}>{request.status ?? '-'}</StatusBadge>
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </Card>
    </section>
  )
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <Card className="bg-panel">
      <CardHeader className="mb-2">
        <CardDescription className="uppercase tracking-wide">{label}</CardDescription>
      </CardHeader>
      <p className="text-2xl font-semibold text-foreground">{value}</p>
    </Card>
  )
}

function Info({ text, danger = false }: { text: string; danger?: boolean }) {
  return (
    <div
      className={cn(
        'rounded-xl border px-4 py-3 text-sm',
        danger
          ? 'border-status-crit bg-panel text-status-crit'
          : 'border-border bg-panel text-muted'
      )}
    >
      {text}
    </div>
  )
}
