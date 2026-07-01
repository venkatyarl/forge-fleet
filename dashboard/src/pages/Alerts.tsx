import { useCallback, useEffect, useMemo, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { cn } from '../lib/utils'

type AlertPolicy = {
  id: string
  name: string
  description?: string
  metric: string
  scope: string
  scope_computer_id?: string
  condition: string
  duration_secs?: number
  severity: string
  cooldown_secs?: number
  channel?: string
  enabled: boolean
  created_at?: string
}

type AlertEvent = {
  id: string
  policy_id: string
  policy_name: string
  severity: string
  metric: string
  computer_id?: string
  computer_name?: string
  fired_at: string
  resolved_at?: string
  value?: number
  value_text?: string
  message?: string
  channel_result?: string
}

type PoliciesPayload = { policies: AlertPolicy[] }
type EventsPayload = { events: AlertEvent[] }

function severityTone(severity: string) {
  const s = severity.toLowerCase()
  if (s === 'critical' || s === 'crit') return 'crit'
  if (s === 'warning' || s === 'warn') return 'warn'
  if (s === 'info') return 'info'
  return 'neutral'
}

function fmtTs(raw?: string) {
  if (!raw) return '-'
  try {
    return new Date(raw).toLocaleString()
  } catch {
    return raw
  }
}

export function Alerts() {
  const [policies, setPolicies] = useState<AlertPolicy[]>([])
  const [events, setEvents] = useState<AlertEvent[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [activeOnly, setActiveOnly] = useState(true)

  const load = useCallback(async () => {
    try {
      setError(null)
      const [policiesRes, eventsRes] = await Promise.all([
        fetch('/api/alerts/policies').then((r) => r.json() as Promise<PoliciesPayload>),
        fetch(`/api/alerts/events?active=${activeOnly}&limit=200`).then((r) => r.json() as Promise<EventsPayload>),
      ])
      setPolicies(policiesRes.policies || [])
      setEvents(eventsRes.events || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load alerts')
    } finally {
      setLoading(false)
    }
  }, [activeOnly])

  useEffect(() => {
    setLoading(true)
    void load()
    const id = window.setInterval(() => void load(), 10000)
    return () => window.clearInterval(id)
  }, [load])

  const activeEvents = useMemo(() => events.filter((e) => !e.resolved_at), [events])
  const criticalCount = useMemo(
    () => activeEvents.filter((e) => e.severity.toLowerCase().includes('crit')).length,
    [activeEvents],
  )
  const warningCount = useMemo(
    () => activeEvents.filter((e) => e.severity.toLowerCase().includes('warn')).length,
    [activeEvents],
  )

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Alerts & Notify</h1>
            {loading ? <Badge variant="info">loading</Badge> : <Badge variant="ok">live</Badge>}
          </div>
          <p className="mt-1 text-sm text-muted">
            Alert policies and fired events from the fleet pulse subsystem.
          </p>
        </div>
        <Button onClick={() => void load()} disabled={loading} type="button" variant="outline">
          Refresh
        </Button>
      </div>

      <div className="grid gap-3 sm:grid-cols-4">
        <SummaryCard label="Policies" value={policies.length.toLocaleString()} detail="configured rules" />
        <SummaryCard label="Active Events" value={activeEvents.length.toLocaleString()} detail="unresolved" />
        <SummaryCard label="Critical" value={criticalCount.toLocaleString()} detail="active critical" tone="crit" />
        <SummaryCard label="Warning" value={warningCount.toLocaleString()} detail="active warnings" tone="warn" />
      </div>

      {error ? (
        <Card className="border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">{error}</Card>
      ) : null}

      <div className="grid gap-4 xl:grid-cols-2">
        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Policies</CardTitle>
              <CardDescription>{policies.length} rule{policies.length === 1 ? '' : 's'}</CardDescription>
            </div>
          </CardHeader>
          {loading && policies.length === 0 ? (
            <p className="text-sm text-dim">Loading policies...</p>
          ) : policies.length === 0 ? (
            <p className="text-sm text-dim">No alert policies configured.</p>
          ) : (
            <div className="space-y-2">
              {policies.map((p) => (
                <div
                  key={p.id}
                  className={cn(
                    'rounded-lg border p-3',
                    p.enabled ? 'border-border bg-surface' : 'border-border-subtle bg-panel opacity-60',
                  )}
                >
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <p className="font-medium text-foreground">{p.name}</p>
                    <div className="flex items-center gap-2">
                      <StatusBadge status={severityTone(p.severity)}>{p.severity}</StatusBadge>
                      <Badge variant={p.enabled ? 'ok' : 'neutral'}>{p.enabled ? 'enabled' : 'disabled'}</Badge>
                    </div>
                  </div>
                  {p.description ? <p className="mt-1 text-xs text-muted">{p.description}</p> : null}
                  <div className="mt-2 flex flex-wrap gap-2 text-xs text-dim">
                    <Badge variant="neutral">{p.metric}</Badge>
                    <Badge variant="neutral">{p.scope}</Badge>
                    {p.condition ? <Badge variant="neutral">{p.condition}</Badge> : null}
                    {p.channel ? <Badge variant="neutral">→ {p.channel}</Badge> : null}
                  </div>
                </div>
              ))}
            </div>
          )}
        </Card>

        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Events</CardTitle>
              <CardDescription>{events.length} event{events.length === 1 ? '' : 's'}</CardDescription>
            </div>
            <Button
              type="button"
              variant={activeOnly ? 'default' : 'outline'}
              size="sm"
              onClick={() => setActiveOnly((v) => !v)}
            >
              {activeOnly ? 'Active only' : 'All events'}
            </Button>
          </CardHeader>
          {loading && events.length === 0 ? (
            <p className="text-sm text-dim">Loading events...</p>
          ) : events.length === 0 ? (
            <p className="text-sm text-dim">No {activeOnly ? 'active' : ''} alert events.</p>
          ) : (
            <div className="max-h-[640px] space-y-2 overflow-y-auto pr-1">
              {events.map((e) => (
                <div
                  key={e.id}
                  className={cn(
                    'rounded-lg border p-3',
                    e.resolved_at ? 'border-border-subtle bg-panel' : 'border-border bg-surface',
                  )}
                >
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <p className="font-medium text-foreground">{e.policy_name}</p>
                    <div className="flex items-center gap-2">
                      <StatusBadge status={severityTone(e.severity)}>{e.severity}</StatusBadge>
                      {e.resolved_at ? <Badge variant="ok">resolved</Badge> : <Badge variant="crit">firing</Badge>}
                    </div>
                  </div>
                  <p className="mt-1 text-xs text-dim">
                    {e.computer_name ?? e.computer_id ?? 'fleet'} · {fmtTs(e.fired_at)}
                  </p>
                  {e.message ? <p className="mt-2 text-sm text-muted">{e.message}</p> : null}
                  {e.value_text ? <p className="text-xs text-dim">Value: {e.value_text}</p> : null}
                  {e.channel_result ? <p className="text-xs text-dim">Notify: {e.channel_result}</p> : null}
                </div>
              ))}
            </div>
          )}
        </Card>
      </div>
    </section>
  )
}

function SummaryCard({
  label,
  value,
  detail,
  tone,
}: {
  label: string
  value: string
  detail: string
  tone?: 'crit' | 'warn'
}) {
  return (
    <Card className="bg-panel px-4 py-3">
      <CardDescription className="uppercase tracking-wide">{label}</CardDescription>
      <div
        className={cn(
          'mt-1 text-2xl font-semibold',
          tone === 'crit' && 'text-status-crit',
          tone === 'warn' && 'text-status-warn',
          !tone && 'text-foreground',
        )}
      >
        {value}
      </div>
      <p className="mt-1 text-xs text-dim">{detail}</p>
    </Card>
  )
}
