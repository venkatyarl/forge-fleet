'use client'

import { useQuery } from '@tanstack/react-query'
import { formatDistanceToNowStrict } from 'date-fns'
import { GitBranch, RefreshCw, TriangleAlert } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { StandaloneTopBar } from '@/components/standalone-top-bar'
import { getJson } from '@/lib/api'
import { cn } from '@/lib/utils'

/* The /api/workstreams endpoint is new and its shape may drift slightly —
   every field is read defensively with fallbacks. */

type WorkstreamClient = {
  node?: string
  tool?: string
  goal?: string
  last_report_at?: string
  attached?: boolean
}

type Workstream = {
  project_key?: string
  name?: string
  status?: string
  working_summary?: string
  remote?: string
  clients?: WorkstreamClient[]
}

type WorkstreamsResponse = { workstreams?: Workstream[] } | Workstream[]

const ACTIVE_WINDOW_MS = 15 * 60 * 1000

function statusVariant(status: string): 'ok' | 'warn' | 'info' | 'neutral' {
  const s = status.toLowerCase()
  if (s === 'active' || s === 'open' || s === 'in_progress' || s === 'in-progress') return 'ok'
  if (s === 'paused' || s === 'stalled' || s === 'blocked') return 'warn'
  if (s === 'done' || s === 'closed' || s === 'completed') return 'info'
  return 'neutral'
}

function parseReportTime(value?: string): Date | null {
  if (!value) return null
  const date = new Date(value)
  return Number.isNaN(date.getTime()) ? null : date
}

function ClientRow({ client, now }: { client: WorkstreamClient; now: number }) {
  const reported = parseReportTime(client.last_report_at)
  const active = reported !== null && now - reported.getTime() < ACTIVE_WINDOW_MS

  return (
    <li className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-lg border border-border bg-surface px-3 py-2.5">
      <span
        className={cn(
          'ff-pulse-dot h-1.5 w-1.5 shrink-0 rounded-full',
          active ? 'bg-status-ok' : 'bg-dim'
        )}
        title={active ? 'reported within the last 15 minutes' : 'no recent report'}
      />
      <span className="font-mono text-xs font-medium text-foreground">
        {client.node ?? 'unknown-node'}
      </span>
      {client.tool ? (
        <Badge variant="default" className="font-mono">
          {client.tool}
        </Badge>
      ) : null}
      {client.attached === false ? <Badge variant="neutral">detached</Badge> : null}
      <span className="min-w-0 flex-1 truncate text-xs text-muted" title={client.goal ?? ''}>
        {client.goal || '—'}
      </span>
      <span className="shrink-0 font-mono text-2xs text-dim">
        {reported ? `${formatDistanceToNowStrict(reported, { addSuffix: true })}` : 'never reported'}
      </span>
    </li>
  )
}

function WorkstreamCard({ ws, now }: { ws: Workstream; now: number }) {
  const title = ws.name ?? ws.project_key ?? 'unnamed'
  const status = ws.status ?? 'unknown'
  const clients = Array.isArray(ws.clients) ? ws.clients : []
  const activeCount = clients.filter((c) => {
    const reported = parseReportTime(c.last_report_at)
    return reported !== null && now - reported.getTime() < ACTIVE_WINDOW_MS
  }).length

  return (
    <Card className="p-5">
      <div className="mb-2 flex flex-wrap items-center gap-2">
        <h2 className="text-sm font-semibold text-foreground">{title}</h2>
        <Badge variant={statusVariant(status)}>{status}</Badge>
        {ws.project_key && ws.project_key !== title ? (
          <span className="font-mono text-2xs text-dim">{ws.project_key}</span>
        ) : null}
        <span className="ml-auto text-2xs text-dim">
          {activeCount} active · {clients.length} attached
        </span>
      </div>
      {ws.working_summary ? (
        <p className="mb-4 text-xs leading-relaxed text-muted">{ws.working_summary}</p>
      ) : (
        <p className="mb-4 text-xs text-dim italic">No working summary yet.</p>
      )}
      {clients.length > 0 ? (
        <ul className="space-y-2">
          {clients.map((client, idx) => (
            <ClientRow key={`${client.node ?? 'node'}-${client.tool ?? 'tool'}-${idx}`} client={client} now={now} />
          ))}
        </ul>
      ) : (
        <p className="text-2xs text-dim">No clients attached.</p>
      )}
    </Card>
  )
}

export default function WorkstreamsPage() {
  const query = useQuery({
    queryKey: ['workstreams'],
    refetchInterval: 15_000,
    retry: 1,
    queryFn: async () => {
      const payload = await getJson<WorkstreamsResponse>('/api/workstreams')
      const list = Array.isArray(payload) ? payload : (payload?.workstreams ?? [])
      return Array.isArray(list) ? list : []
    },
  })

  const workstreams = query.data ?? []
  // dataUpdatedAt bumps on every successful refetch (15s poll) — the pure
  // "now" for active-window checks (Date.now() in render breaks purity).
  const now = query.dataUpdatedAt

  return (
    <div className="min-h-screen bg-background">
      <StandaloneTopBar title="Workstreams" subtitle="Live session-of-record across fleet CLIs">
        <Badge variant={query.isError ? 'crit' : query.isSuccess ? 'ok' : 'info'}>
          {query.isError ? 'unavailable' : query.isSuccess ? 'live' : 'loading'}
        </Badge>
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => void query.refetch()}
          disabled={query.isFetching}
        >
          <RefreshCw className={cn('h-3.5 w-3.5', query.isFetching && 'animate-spin')} />
          Refresh
        </Button>
      </StandaloneTopBar>

      <main className="mx-auto max-w-6xl px-4 py-8 sm:px-6">
        {query.isError ? (
          <Card className="flex flex-col items-center gap-3 py-16 text-center">
            <TriangleAlert className="h-8 w-8 text-status-warn" />
            <p className="text-sm font-medium text-foreground">Workstreams endpoint unavailable</p>
            <p className="max-w-md text-xs text-dim">
              <span className="font-mono">GET /api/workstreams</span> returned{' '}
              <span className="font-mono">
                {query.error instanceof Error ? query.error.message : 'an error'}
              </span>
              . The gateway may not serve this endpoint yet — the page retries every 15s and will
              fill in once it is live.
            </p>
          </Card>
        ) : query.isSuccess && workstreams.length === 0 ? (
          <Card className="flex flex-col items-center gap-3 py-16 text-center">
            <GitBranch className="h-8 w-8 text-dim" />
            <p className="text-sm font-medium text-foreground">No workstreams yet</p>
            <p className="max-w-md text-xs text-dim">
              Attach a CLI session with <span className="font-mono">ff workstream attach</span> and
              it will show up here.
            </p>
          </Card>
        ) : (
          <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
            {query.isLoading
              ? Array.from({ length: 2 }, (_, i) => (
                  <Card key={i} className="h-40 animate-pulse bg-panel/60" />
                ))
              : workstreams.map((ws, idx) => (
                  <WorkstreamCard key={ws.project_key ?? ws.name ?? idx} ws={ws} now={now} />
                ))}
          </div>
        )}
      </main>
    </div>
  )
}
