import { useCallback, useEffect, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardHeader, CardTitle, CardDescription } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { getJson } from '../lib/api'
import { extractModels } from '../lib/normalizers'
import { cn } from '../lib/utils'
import type { FleetModel, FleetStatusResponse } from '../types'

export function ModelInventory() {
  const [models, setModels] = useState<FleetModel[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const payload = await getJson<FleetStatusResponse>('/api/models').catch(() =>
        getJson<FleetStatusResponse>('/v1/models').catch(() => getJson<FleetStatusResponse>('/api/fleet/status')),
      )
      setModels(extractModels(payload))
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load model inventory')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  const loadedCount = models.filter((model) => {
    const status = model.status?.toLowerCase()
    return !status || ['loaded', 'online', 'active', 'ready', 'healthy'].includes(status)
  }).length

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-xl font-semibold text-foreground">Model Inventory</h2>
            {loading ? <Badge variant="info">loading</Badge> : <Badge variant="ok">live</Badge>}
          </div>
          <p className="mt-1 text-sm text-muted">
            {loading
              ? 'Loading model inventory from the fleet gateway.'
              : `${models.length} model${models.length !== 1 ? 's' : ''} reported across fleet nodes.`}
          </p>
        </div>
        <Button onClick={() => void load()} type="button" variant="outline" disabled={loading}>
          Refresh
        </Button>
      </div>

      {loading ? <Info text="Loading models..." /> : null}
      {error ? <Info text={`Error: ${error}`} danger /> : null}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        <SummaryCard label="Reported Models" value={models.length.toLocaleString()} detail="from /api/models fallback chain" />
        <SummaryCard label="Loaded Models" value={loadedCount.toLocaleString()} detail="ready or status-unreported rows" />
        <SummaryCard
          label="Fleet Nodes"
          value={new Set(models.map((model) => model.node).filter(Boolean)).size.toLocaleString()}
          detail="nodes with model metadata"
        />
      </div>

      <Card className="overflow-hidden bg-surface p-0">
        <CardHeader className="mb-0 border-b border-border px-4 py-3">
          <div>
            <CardTitle>Loaded Models</CardTitle>
            <CardDescription>Model name, serving tier, node placement, and context window.</CardDescription>
          </div>
          <Badge variant="neutral">{models.length} rows</Badge>
        </CardHeader>
        <div className="overflow-x-auto">
          <table className="min-w-full text-left text-sm">
            <thead className="border-b border-border bg-elevated text-xs uppercase text-dim">
              <tr>
                <th className="px-4 py-2 font-medium">Model</th>
                <th className="px-4 py-2 font-medium">Tier</th>
                <th className="px-4 py-2 font-medium">Status</th>
                <th className="px-4 py-2 font-medium">Node</th>
                <th className="px-4 py-2 font-medium">Context</th>
              </tr>
            </thead>
            <tbody>
              {models.length === 0 ? (
                <tr>
                  <td className="px-4 py-8 text-center text-sm text-dim" colSpan={5}>
                    {loading ? 'Loading models...' : 'No models reported by API yet.'}
                  </td>
                </tr>
              ) : (
                models.map((model, idx) => (
                  <tr
                    key={`${model.id ?? model.name}-${idx}`}
                    className="border-t border-border text-muted transition hover:bg-panel hover:text-foreground"
                  >
                    <td className="px-4 py-3 font-mono text-xs text-status-info">{model.name}</td>
                    <td className="px-4 py-3">
                      {model.tier == null ? '-' : <Badge variant="neutral">tier {model.tier}</Badge>}
                    </td>
                    <td className="px-4 py-3">
                      <StatusBadge status={model.status ?? 'unknown'}>{model.status ?? 'unknown'}</StatusBadge>
                    </td>
                    <td className="px-4 py-3">
                      {model.node ? <Badge variant="neutral">{model.node}</Badge> : '-'}
                    </td>
                    <td className="whitespace-nowrap px-4 py-3 text-foreground">
                      {formatContext(model.contextWindow)}
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

function SummaryCard({ label, value, detail }: { label: string; value: string; detail: string }) {
  return (
    <Card className="bg-panel">
      <CardHeader className="mb-2">
        <CardDescription className="uppercase tracking-wide">{label}</CardDescription>
      </CardHeader>
      <div className="text-2xl font-semibold text-foreground">{value}</div>
      <p className="mt-1 text-xs text-dim">{detail}</p>
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

function formatContext(contextWindow: number | undefined) {
  return contextWindow == null ? '-' : contextWindow.toLocaleString()
}
