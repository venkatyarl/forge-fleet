import { useCallback, useEffect, useMemo, useState } from 'react'
import { useOutletContext } from 'react-router-dom'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { Badge } from '../components/ui/badge'
import { StatusBadge } from '../components/ui/status-badge'
import { Button } from '../components/ui/button'
import { getJson } from '../lib/api'
import { cn } from '../lib/utils'
import type { WsEvent } from '../types'

/* -- types -------------------------------------------------- */

type AuditEntry = {
  id?: string | number
  timestamp?: string
  created_at?: string
  actor?: string
  action?: string
  event_type?: string
  target?: string | null
  details?: unknown
  details_json?: string
  severity?: string
  level?: string
  node?: string
  worker_name?: string
  [key: string]: unknown
}

type AuditResponse =
  | {
      events?: AuditEntry[]
      entries?: AuditEntry[]
      items?: AuditEntry[]
      [key: string]: unknown
    }
  | AuditEntry[]

type BadgeTone = 'ok' | 'warn' | 'crit' | 'info' | 'neutral'
type TimeRange = 'all' | '1h' | '24h' | '7d' | 'custom'

/* -- data helpers ------------------------------------------- */

function extractEntries(data: AuditResponse | null): AuditEntry[] {
  if (Array.isArray(data)) return data
  return data?.events ?? data?.entries ?? data?.items ?? []
}

function firstString(...values: unknown[]): string | null {
  for (const value of values) {
    if (typeof value === 'string' && value.trim()) return value
  }
  return null
}

function parseMaybeJson(value: unknown): unknown {
  if (typeof value !== 'string') return value
  try {
    return JSON.parse(value)
  } catch {
    return value
  }
}

function asRecord(value: unknown): Record<string, unknown> | null {
  const parsed = parseMaybeJson(value)
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return null
  return parsed as Record<string, unknown>
}

function eventType(entry: AuditEntry): string {
  return firstString(entry.action, entry.event_type, entry.type) ?? 'unknown'
}

function eventTypeBase(entry: AuditEntry): string {
  return eventType(entry).split(/[._:]/)[0].toLowerCase()
}

function eventTimestamp(entry: AuditEntry): string {
  return firstString(entry.timestamp, entry.created_at) ?? ''
}

function eventActor(entry: AuditEntry): string {
  return firstString(entry.actor, entry.user, entry.worker_name, entry.node) ?? 'system'
}

function eventTarget(entry: AuditEntry): string {
  return firstString(entry.target, entry.node, entry.worker_name) ?? '-'
}

function normalizeSeverity(value: string | null): string {
  const severity = value?.toLowerCase().trim()
  if (!severity) return 'info'
  if (['critical', 'crit', 'error', 'failed', 'failure', 'denied', 'blocked'].includes(severity)) {
    return 'critical'
  }
  if (['warn', 'warning', 'degraded', 'retry', 'requeued'].includes(severity)) return 'warning'
  if (['ok', 'success', 'succeeded', 'passed', 'info', 'notice', 'debug'].includes(severity)) return 'info'
  return severity
}

function eventSeverity(entry: AuditEntry): string {
  const details = asRecord(entry.details ?? entry.details_json)
  const explicit = firstString(
    entry.severity,
    entry.level,
    details?.severity,
    details?.level,
    details?.outcome
  )
  const normalized = normalizeSeverity(explicit)
  if (normalized !== 'info' || explicit) return normalized

  const action = eventType(entry).toLowerCase()
  if (/(critical|error|failed|failure|denied|blocked)/.test(action)) return 'critical'
  if (/(warn|warning|degraded|retry|requeued)/.test(action)) return 'warning'
  return 'info'
}

function eventDetails(entry: AuditEntry): string {
  const value = entry.details ?? entry.details_json
  const parsed = parseMaybeJson(value)
  if (parsed == null || parsed === '') return '-'
  if (typeof parsed === 'string') return parsed
  if (typeof parsed === 'number' || typeof parsed === 'boolean') return String(parsed)

  if (typeof parsed === 'object') {
    const record = parsed as Record<string, unknown>
    for (const key of ['message', 'reason', 'summary', 'raw', 'error', 'status']) {
      const field = record[key]
      if (typeof field === 'string' || typeof field === 'number' || typeof field === 'boolean') {
        return `${key}: ${String(field)}`
      }
    }
  }

  return JSON.stringify(parsed)
}

function actionTypes(entries: AuditEntry[]): string[] {
  const set = new Set<string>()
  for (const entry of entries) set.add(eventTypeBase(entry))
  return Array.from(set).sort()
}

function severityTypes(entries: AuditEntry[]): string[] {
  const set = new Set<string>()
  for (const entry of entries) set.add(eventSeverity(entry))
  return Array.from(set).sort()
}

function fmtTime(iso: string): string {
  if (!iso) return '-'
  const time = Date.parse(iso)
  if (Number.isNaN(time)) return iso
  return new Date(time).toLocaleString()
}

function withinTimeRange(entry: AuditEntry, range: TimeRange, dateFrom: string, dateTo: string): boolean {
  if (range === 'all') return true

  const time = Date.parse(eventTimestamp(entry))
  if (Number.isNaN(time)) return false

  if (range === 'custom') {
    if (dateFrom && time < new Date(dateFrom).getTime()) return false
    if (dateTo) {
      const end = new Date(dateTo)
      end.setDate(end.getDate() + 1)
      if (time >= end.getTime()) return false
    }
    return true
  }

  const durationMs =
    range === '1h'
      ? 60 * 60 * 1000
      : range === '24h'
        ? 24 * 60 * 60 * 1000
        : 7 * 24 * 60 * 60 * 1000

  return time >= Date.now() - durationMs
}

function actionTone(action: string): BadgeTone {
  const lower = action.toLowerCase()
  if (/(failed|failure|error|denied|blocked|auth)/.test(lower)) return 'crit'
  if (/(warn|warning|degraded|retry|update)/.test(lower)) return 'warn'
  if (/(config|model|node|deploy|task|leader)/.test(lower)) return 'info'
  if (/(success|done|ready|created)/.test(lower)) return 'ok'
  return 'neutral'
}

function statusToneClass(tone: BadgeTone): string {
  if (tone === 'ok') return 'text-status-ok'
  if (tone === 'warn') return 'text-status-warn'
  if (tone === 'crit') return 'text-status-crit'
  if (tone === 'info') return 'text-status-info'
  return 'text-muted'
}

/* -- component ---------------------------------------------- */

export function AuditLog() {
  const { wsEvent } = useOutletContext<{ wsEvent: WsEvent | null }>()
  const [entries, setEntries] = useState<AuditEntry[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null)

  const [filterType, setFilterType] = useState('')
  const [filterSeverity, setFilterSeverity] = useState('')
  const [filterActor, setFilterActor] = useState('')
  const [timeRange, setTimeRange] = useState<TimeRange>('all')
  const [dateFrom, setDateFrom] = useState('')
  const [dateTo, setDateTo] = useState('')
  const [liveTail, setLiveTail] = useState(false)

  const load = useCallback(async () => {
    try {
      setLoading(true)
      setError(null)
      let data: AuditResponse | null = null
      try {
        data = await getJson<AuditResponse>('/api/audit/recent')
      } catch {
        data = await getJson<AuditResponse>('/api/audit/events')
      }
      setEntries(extractEntries(data))
      setLastUpdated(new Date())
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load audit log')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  useEffect(() => {
    if (!liveTail) return undefined
    const interval = window.setInterval(() => void load(), 5000)
    return () => window.clearInterval(interval)
  }, [liveTail, load])

  useEffect(() => {
    if (liveTail && wsEvent?.type.toLowerCase().includes('audit')) void load()
  }, [liveTail, load, wsEvent])

  const types = useMemo(() => actionTypes(entries), [entries])
  const severities = useMemo(() => severityTypes(entries), [entries])

  const filtered = useMemo(() => {
    return entries.filter((entry) => {
      if (filterType && eventTypeBase(entry) !== filterType) return false
      if (filterSeverity && eventSeverity(entry) !== filterSeverity) return false
      if (filterActor && !eventActor(entry).toLowerCase().includes(filterActor.toLowerCase())) return false
      return withinTimeRange(entry, timeRange, dateFrom, dateTo)
    })
  }, [entries, filterActor, filterSeverity, filterType, timeRange, dateFrom, dateTo])

  const criticalCount = useMemo(
    () => entries.filter((entry) => eventSeverity(entry) === 'critical').length,
    [entries]
  )
  const latestEntry = filtered[0]
  const filtersActive = Boolean(filterType || filterSeverity || filterActor || timeRange !== 'all' || dateFrom || dateTo)

  const clearFilters = () => {
    setFilterType('')
    setFilterSeverity('')
    setFilterActor('')
    setTimeRange('all')
    setDateFrom('')
    setDateTo('')
  }

  return (
    <section className="min-h-full space-y-6 bg-background">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Audit Log</h1>
            <StatusBadge status={liveTail ? 'active' : 'standby'}>
              {liveTail ? 'live tail on' : 'live tail off'}
            </StatusBadge>
          </div>
          <p className="mt-1 text-sm text-dim">
            Fleet events, security actions, and operational changes
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Button
            type="button"
            variant={liveTail ? 'default' : 'outline'}
            onClick={() => setLiveTail((value) => !value)}
          >
            Live tail
          </Button>
          <Button type="button" variant="outline" onClick={() => void load()} disabled={loading}>
            {loading ? 'Loading...' : 'Refresh'}
          </Button>
        </div>
      </div>

      {error ? (
        <Card className="border-border bg-panel">
          <div className="text-sm text-status-crit">{error}</div>
        </Card>
      ) : null}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <MetricCard label="Total Events" value={entries.length} tone="info" />
        <MetricCard label="Visible" value={filtered.length} tone="ok" />
        <MetricCard label="Critical" value={criticalCount} tone={criticalCount > 0 ? 'crit' : 'neutral'} />
        <MetricCard label="Types" value={types.length} tone="neutral" />
      </div>

      <Card className="bg-panel">
        <CardHeader className="gap-3">
          <div>
            <CardTitle>Filters</CardTitle>
            <CardDescription>Type, severity, actor, and time range</CardDescription>
          </div>
          {filtersActive ? (
            <Button type="button" variant="ghost" size="sm" onClick={clearFilters}>
              Clear
            </Button>
          ) : null}
        </CardHeader>

        <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-5">
          <label className="space-y-1">
            <span className="text-xs font-medium uppercase text-dim">Type</span>
            <select
              value={filterType}
              onChange={(event) => setFilterType(event.target.value)}
              className="h-9 w-full rounded-lg border border-border bg-elevated px-3 text-sm text-foreground focus:border-primary focus:outline-hidden"
            >
              <option value="">All types</option>
              {types.map((type) => (
                <option key={type} value={type}>
                  {type}
                </option>
              ))}
            </select>
          </label>

          <label className="space-y-1">
            <span className="text-xs font-medium uppercase text-dim">Severity</span>
            <select
              value={filterSeverity}
              onChange={(event) => setFilterSeverity(event.target.value)}
              className="h-9 w-full rounded-lg border border-border bg-elevated px-3 text-sm text-foreground focus:border-primary focus:outline-hidden"
            >
              <option value="">All severities</option>
              {severities.map((severity) => (
                <option key={severity} value={severity}>
                  {severity}
                </option>
              ))}
            </select>
          </label>

          <label className="space-y-1">
            <span className="text-xs font-medium uppercase text-dim">Actor</span>
            <input
              type="text"
              placeholder="system, operator, node"
              value={filterActor}
              onChange={(event) => setFilterActor(event.target.value)}
              className="h-9 w-full rounded-lg border border-border bg-elevated px-3 text-sm text-foreground placeholder:text-dim focus:border-primary focus:outline-hidden"
            />
          </label>

          <label className="space-y-1">
            <span className="text-xs font-medium uppercase text-dim">Range</span>
            <select
              value={timeRange}
              onChange={(event) => setTimeRange(event.target.value as TimeRange)}
              className="h-9 w-full rounded-lg border border-border bg-elevated px-3 text-sm text-foreground focus:border-primary focus:outline-hidden"
            >
              <option value="all">All time</option>
              <option value="1h">Last hour</option>
              <option value="24h">Last 24 hours</option>
              <option value="7d">Last 7 days</option>
              <option value="custom">Custom dates</option>
            </select>
          </label>

          <div className="grid grid-cols-2 gap-2">
            <label className="space-y-1">
              <span className="text-xs font-medium uppercase text-dim">From</span>
              <input
                type="date"
                value={dateFrom}
                onChange={(event) => {
                  setDateFrom(event.target.value)
                  setTimeRange('custom')
                }}
                className="h-9 w-full rounded-lg border border-border bg-elevated px-2 text-sm text-foreground focus:border-primary focus:outline-hidden"
              />
            </label>
            <label className="space-y-1">
              <span className="text-xs font-medium uppercase text-dim">To</span>
              <input
                type="date"
                value={dateTo}
                onChange={(event) => {
                  setDateTo(event.target.value)
                  setTimeRange('custom')
                }}
                className="h-9 w-full rounded-lg border border-border bg-elevated px-2 text-sm text-foreground focus:border-primary focus:outline-hidden"
              />
            </label>
          </div>
        </div>
      </Card>

      <Card className="overflow-hidden bg-panel p-0">
        <CardHeader className="mb-0 border-b border-border px-4 py-3">
          <div>
            <CardTitle>Events</CardTitle>
            <CardDescription>
              {lastUpdated ? `Updated ${lastUpdated.toLocaleTimeString()}` : 'Waiting for audit data'}
            </CardDescription>
          </div>
          <Badge variant="neutral">
            Showing {filtered.length} of {entries.length}
          </Badge>
        </CardHeader>

        {loading && entries.length === 0 ? (
          <EmptyState text="Loading audit log..." />
        ) : entries.length === 0 ? (
          <EmptyState text="No audit events found" />
        ) : (
          <div className="overflow-x-auto">
            <table className="min-w-full text-left text-sm">
              <thead className="border-b border-border bg-elevated text-xs uppercase text-dim">
                <tr>
                  <th className="px-4 py-3 font-medium">Timestamp</th>
                  <th className="px-4 py-3 font-medium">Severity</th>
                  <th className="px-4 py-3 font-medium">Actor</th>
                  <th className="px-4 py-3 font-medium">Type</th>
                  <th className="px-4 py-3 font-medium">Target</th>
                  <th className="px-4 py-3 font-medium">Details</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-border">
                {filtered.length === 0 ? (
                  <tr>
                    <td colSpan={6} className="px-4 py-10 text-center text-sm text-dim">
                      No events match current filters
                    </td>
                  </tr>
                ) : (
                  filtered.map((entry, index) => {
                    const action = eventType(entry)
                    const details = eventDetails(entry)
                    const tone = actionTone(action)
                    const isLatest = latestEntry === entry && liveTail

                    return (
                      <tr
                        key={entry.id ?? `${eventTimestamp(entry)}-${index}`}
                        className={cn(
                          'transition hover:bg-primary-subtle',
                          isLatest && 'bg-primary-subtle'
                        )}
                      >
                        <td className="whitespace-nowrap px-4 py-3 text-xs text-dim">
                          {fmtTime(eventTimestamp(entry))}
                        </td>
                        <td className="px-4 py-3">
                          <StatusBadge status={eventSeverity(entry)} />
                        </td>
                        <td className="px-4 py-3 font-medium text-foreground">{eventActor(entry)}</td>
                        <td className="px-4 py-3">
                          <Badge variant={tone}>{action}</Badge>
                        </td>
                        <td className="px-4 py-3 text-muted">{eventTarget(entry)}</td>
                        <td className="max-w-sm truncate px-4 py-3 text-xs text-dim" title={details}>
                          {details}
                        </td>
                      </tr>
                    )
                  })
                )}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </section>
  )
}

function MetricCard({ label, value, tone }: { label: string; value: string | number; tone: BadgeTone }) {
  return (
    <Card className="bg-panel">
      <CardHeader className="mb-2">
        <CardDescription>{label}</CardDescription>
      </CardHeader>
      <div className={cn('text-2xl font-bold', statusToneClass(tone))}>{value}</div>
    </Card>
  )
}

function EmptyState({ text }: { text: string }) {
  return <div className="flex h-48 items-center justify-center text-sm text-dim">{text}</div>
}
