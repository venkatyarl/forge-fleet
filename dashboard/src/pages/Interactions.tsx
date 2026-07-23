import { useCallback, useEffect, useMemo, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { cn } from '../lib/utils'

type Interaction = {
  id?: string | number
  channel: string
  engine?: string
  request_text: string
  response_text?: string
  error_text?: string
  outcome?: string
  latency_ms?: number
  tokens_in?: number
  tokens_out?: number
  ts?: string
  created_at?: string
}

type SummaryPayload = {
  channels: { channel: string; count: number }[]
  total: number
  error?: string
}

type ListPayload = {
  rows: Interaction[]
  error?: string
}

const CHANNELS = ['all', 'mcp', 'cli', 'gateway-jarvis', 'session', 'council_member', 'council_chairman']

function fmtTs(raw?: string) {
  if (!raw) return '-'
  try {
    return new Date(raw).toLocaleString()
  } catch {
    return raw
  }
}

function totalTokens(row: Interaction) {
  return (row.tokens_in ?? 0) + (row.tokens_out ?? 0)
}

function outcomeTone(outcome?: string) {
  const o = (outcome ?? '').toLowerCase()
  if (o === 'success' || o === 'ok') return 'ok'
  if (o === 'error' || o === 'failure' || o === 'failed') return 'crit'
  return 'neutral'
}

export function Interactions() {
  const [summary, setSummary] = useState<SummaryPayload | null>(null)
  const [rows, setRows] = useState<Interaction[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [channel, setChannel] = useState('all')
  const [expanded, setExpanded] = useState<Set<number>>(new Set())

  const load = useCallback(async () => {
    try {
      setError(null)
      const [sumRes, listRes] = await Promise.all([
        fetch('/api/interactions/summary').then((r) => r.json() as Promise<SummaryPayload>),
        fetch(`/api/interactions?limit=150&channel=${encodeURIComponent(channel)}`).then(
          (r) => r.json() as Promise<ListPayload>,
        ),
      ])
      setSummary(sumRes)
      setRows(listRes.rows || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load interactions')
    } finally {
      setLoading(false)
    }
  }, [channel])

  useEffect(() => {
    setLoading(true)
    void load()
    const id = window.setInterval(() => void load(), 5000)
    return () => window.clearInterval(id)
  }, [load])

  const channelCounts = useMemo(() => {
    const map = new Map<string, number>()
    map.set('all', summary?.total ?? 0)
    summary?.channels.forEach((c) => map.set(c.channel, c.count))
    return map
  }, [summary])

  const toggleExpanded = (idx: number) => {
    setExpanded((prev) => {
      const next = new Set(prev)
      if (next.has(idx)) next.delete(idx)
      else next.add(idx)
      return next
    })
  }

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Interactions / Training</h1>
            {loading ? <Badge variant="info">loading</Badge> : <Badge variant="ok">live</Badge>}
          </div>
          <p className="mt-1 text-sm text-muted">
            Request/response corpus logged to <code className="rounded-sm bg-elevated px-1 font-mono text-primary">ff_interactions</code>. Polls every 5s.
          </p>
        </div>
        <Button onClick={() => void load()} disabled={loading} type="button" variant="outline">
          Refresh
        </Button>
      </div>

      <div className="grid gap-3 sm:grid-cols-3">
        <SummaryCard label="Total Interactions" value={(summary?.total ?? 0).toLocaleString()} detail="across all channels" />
        <SummaryCard label="Rows Loaded" value={rows.length.toLocaleString()} detail="current filter" />
        <SummaryCard
          label="Channels"
          value={(summary?.channels.length ?? 0).toLocaleString()}
          detail="distinct channels"
        />
      </div>

      {error ? (
        <Card className="border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">{error}</Card>
      ) : null}

      <Card className="bg-panel">
        <CardHeader>
          <div>
            <CardTitle>Channel Filter</CardTitle>
            <CardDescription>Filter the interaction stream by channel.</CardDescription>
          </div>
        </CardHeader>
        <div className="flex flex-wrap gap-2">
          {CHANNELS.map((ch) => (
            <Button
              key={ch}
              type="button"
              variant={channel === ch ? 'default' : 'outline'}
              size="sm"
              onClick={() => setChannel(ch)}
            >
              {ch}
              <Badge variant="neutral" className="ml-1.5">
                {channelCounts.get(ch) ?? 0}
              </Badge>
            </Button>
          ))}
        </div>
      </Card>

      <Card className="bg-panel">
        <CardHeader>
          <div>
            <CardTitle>Recent Interactions</CardTitle>
            <CardDescription>
              {rows.length} row{rows.length === 1 ? '' : 's'} · click a row to expand
            </CardDescription>
          </div>
        </CardHeader>
        {loading && rows.length === 0 ? (
          <p className="text-sm text-dim">Loading interactions...</p>
        ) : rows.length === 0 ? (
          <p className="text-sm text-dim">No interactions captured for <span className="font-mono text-primary">{channel}</span>.</p>
        ) : (
          <div className="space-y-2">
            {rows.map((row, idx) => {
              const isOpen = expanded.has(idx)
              const answer = row.outcome === 'error' && row.error_text ? row.error_text : row.response_text
              const tokens = totalTokens(row)
              return (
                <button
                  key={idx}
                  type="button"
                  onClick={() => toggleExpanded(idx)}
                  className={cn(
                    'w-full rounded-lg border p-3 text-left transition',
                    isOpen
                      ? 'border-primary bg-primary-subtle'
                      : 'border-border bg-surface hover:border-border-subtle hover:bg-elevated',
                  )}
                >
                  <div className="flex flex-wrap items-center gap-2">
                    <Badge variant="default">{row.channel}</Badge>
                    {row.engine ? <Badge variant="neutral">{row.engine}</Badge> : null}
                    {row.latency_ms != null ? <Badge variant="neutral">{row.latency_ms} ms</Badge> : null}
                    {tokens > 0 ? <Badge variant="neutral">{tokens} tok</Badge> : null}
                    {row.outcome ? (
                      <StatusBadge status={outcomeTone(row.outcome)}>{row.outcome}</StatusBadge>
                    ) : null}
                    <span className="ml-auto text-xs text-dim">{fmtTs(row.ts ?? row.created_at)}</span>
                  </div>
                  <div className={cn('mt-2 text-sm text-muted', !isOpen && 'line-clamp-3')}>
                    <span className="font-mono text-xs text-dim">▸</span>{' '}
                    {row.request_text || <em className="text-dim">(empty)</em>}
                  </div>
                  {isOpen && (
                    <div className="mt-3 rounded-lg border border-border bg-background p-3 text-sm">
                      <span className="font-mono text-xs text-dim">◂</span>{' '}
                      <span className={cn('whitespace-pre-wrap', row.outcome === 'error' && 'text-status-crit')}>
                        {answer || <em className="text-dim">(empty)</em>}
                      </span>
                    </div>
                  )}
                </button>
              )
            })}
          </div>
        )}
      </Card>
    </section>
  )
}

function SummaryCard({ label, value, detail }: { label: string; value: string; detail: string }) {
  return (
    <Card className="bg-panel px-4 py-3">
      <CardDescription className="uppercase tracking-wide">{label}</CardDescription>
      <div className="mt-1 text-2xl font-semibold text-foreground">{value}</div>
      <p className="mt-1 text-xs text-dim">{detail}</p>
    </Card>
  )
}
