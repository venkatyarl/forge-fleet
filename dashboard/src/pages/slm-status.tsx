import { useCallback, useEffect, useState } from 'react'
import { Card, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'

interface SlmStatusData {
  model: string
  memory_usage_mb: number | null
  thread_count: number | null
  last_ping: string
  online: boolean
}

export function SlmStatus() {
  const [status, setStatus] = useState<SlmStatusData | null>(null)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      const response = await fetch('/slm/status')
      if (!response.ok) throw new Error(`Status request failed (${response.status})`)
      setStatus(await response.json() as SlmStatusData)
      setError(null)
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : 'Failed to load SLM status')
    }
  }, [])

  useEffect(() => {
    void refresh()
    const timer = window.setInterval(() => void refresh(), 30_000)
    return () => window.clearInterval(timer)
  }, [refresh])

  return (
    <div className="mx-auto max-w-2xl space-y-4">
      <div>
        <h1 className="text-2xl font-bold text-foreground">SLM Status</h1>
        <p className="mt-1 text-sm text-muted">Local small-language-model runtime health.</p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>{status?.model ?? 'Local SLM'}</CardTitle>
          <StatusBadge status={status?.online ? 'online' : 'offline'}>
            {status?.online ? 'online' : 'offline'}
          </StatusBadge>
        </CardHeader>
        {error ? (
          <p className="text-sm text-danger">{error}</p>
        ) : (
          <dl className="grid gap-4 text-sm sm:grid-cols-3">
            <StatusField label="Memory usage" value={formatMemory(status?.memory_usage_mb)} />
            <StatusField label="Threads" value={status?.thread_count?.toString() ?? '—'} />
            <StatusField label="Last ping" value={formatTimestamp(status?.last_ping)} />
          </dl>
        )}
      </Card>
    </div>
  )
}

function StatusField({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <dt className="text-xs uppercase tracking-wide text-dim">{label}</dt>
      <dd className="mt-1 font-medium text-foreground">{value}</dd>
    </div>
  )
}

function formatMemory(value?: number | null) {
  return value == null ? '—' : `${value.toLocaleString()} MiB`
}

function formatTimestamp(value?: string) {
  return value ? new Date(value).toLocaleString() : '—'
}
