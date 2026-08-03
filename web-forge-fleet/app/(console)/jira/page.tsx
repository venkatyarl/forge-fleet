'use client'

import { useQuery } from '@tanstack/react-query'
import { formatDistanceToNowStrict } from 'date-fns'
import { Activity, RefreshCw, Ticket } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { getJson } from '@/lib/api'
import { cn } from '@/lib/utils'

/* GET /api/jira/status → { configs: [...], recent_events: [...] }.
   Observed live shape also carries retag_after_s on configs; read defensively. */

type JiraConfig = {
  name?: string
  project_key?: string
  poll_interval_s?: number
  queue_jql?: string
  ruleset_id?: string
  version?: number
  watched_issues?: number
  awaiting_issues?: number
  next_action_at?: string | null
}

type JiraEvent = {
  event_key?: string
  config_id?: string
  issue_id?: string
  kind?: string
  created_at?: string
}

type JiraStatusResponse = {
  configs?: JiraConfig[]
  recent_events?: JiraEvent[]
}

function kindVariant(kind: string): 'info' | 'ok' | 'warn' | 'neutral' {
  const k = kind.toLowerCase()
  if (k === 'reply') return 'info'
  if (k === 'status-change' || k === 'status_change') return 'ok'
  if (k === 'new-assigned' || k === 'new_assigned') return 'warn'
  return 'neutral' // observed and anything unrecognized
}

function relTime(value?: string | null): string {
  if (!value) return '—'
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return '—'
  return formatDistanceToNowStrict(date, { addSuffix: true })
}

function pollLabel(seconds?: number): string {
  if (seconds == null) return '—'
  if (seconds % 60 === 0) return `${seconds / 60}m`
  return `${seconds}s`
}

function ConfigCard({ config }: { config: JiraConfig }) {
  return (
    <Card className="bg-panel p-5">
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <h3 className="text-sm font-semibold text-foreground">{config.name ?? 'unnamed'}</h3>
        {config.project_key ? <Badge variant="default">{config.project_key}</Badge> : null}
        <Badge variant="neutral">poll {pollLabel(config.poll_interval_s)}</Badge>
        {config.ruleset_id ? (
          <span className="font-mono text-2xs text-dim">ruleset {config.ruleset_id}</span>
        ) : null}
        {config.version != null ? (
          <span className="ml-auto font-mono text-2xs text-dim">v{config.version}</span>
        ) : null}
      </div>
      {config.queue_jql ? (
        <pre className="mb-3 overflow-x-auto rounded-lg border border-border bg-background p-2 font-mono text-2xs text-muted">
          {config.queue_jql}
        </pre>
      ) : null}
      <div className="flex flex-wrap items-center gap-x-5 gap-y-1 text-xs text-dim">
        <span>
          <span className="font-semibold text-foreground">{config.watched_issues ?? 0}</span> watched
        </span>
        <span>
          <span className="font-semibold text-foreground">{config.awaiting_issues ?? 0}</span> awaiting
        </span>
        <span className="ml-auto">next action: {relTime(config.next_action_at)}</span>
      </div>
    </Card>
  )
}

export default function JiraPage() {
  const query = useQuery({
    queryKey: ['jira', 'status'],
    refetchInterval: 30_000,
    retry: 1,
    queryFn: async () => {
      const payload = await getJson<JiraStatusResponse>('/api/jira/status')
      return {
        configs: Array.isArray(payload?.configs) ? payload.configs : [],
        events: Array.isArray(payload?.recent_events) ? payload.recent_events : [],
      }
    },
  })

  const configs = query.data?.configs ?? []
  const events = query.data?.events ?? []

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-xl font-semibold text-foreground">Jira Monitor</h2>
            {query.isLoading ? (
              <Badge variant="info">loading</Badge>
            ) : query.isError ? (
              <Badge variant="crit">unavailable</Badge>
            ) : (
              <Badge variant="ok">live</Badge>
            )}
          </div>
          <p className="mt-1 text-sm text-muted">
            Watched queues and recent Jira activity, refreshed every 30s.
          </p>
        </div>
        <Button
          onClick={() => void query.refetch()}
          type="button"
          variant="outline"
          disabled={query.isFetching}
        >
          <RefreshCw className={cn('h-3.5 w-3.5', query.isFetching && 'animate-spin')} />
          Refresh
        </Button>
      </div>

      {query.isError ? (
        <div className="rounded-xl border border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">
          Error: {query.error instanceof Error ? query.error.message : 'failed to load Jira status'}
        </div>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <div className="space-y-4">
          <Card className="bg-surface p-0">
            <CardHeader className="mb-0 border-b border-border px-4 py-3">
              <div>
                <CardTitle>Monitored Configs</CardTitle>
                <CardDescription>Poll interval, queue JQL, and watched/awaiting counts.</CardDescription>
              </div>
              <Badge variant="neutral">{configs.length} config{configs.length === 1 ? '' : 's'}</Badge>
            </CardHeader>
            <div className="space-y-3 p-4">
              {query.isLoading ? (
                Array.from({ length: 2 }, (_, i) => (
                  <div key={i} className="h-32 animate-pulse rounded-xl bg-panel/60" />
                ))
              ) : configs.length === 0 ? (
                <p className="py-8 text-center text-xs text-dim">
                  <Ticket className="mx-auto mb-2 h-6 w-6" />
                  No Jira monitor configs — start one with{' '}
                  <span className="font-mono">ff jira monitor --config &lt;name&gt; --daemon</span>.
                </p>
              ) : (
                configs.map((config, idx) => <ConfigCard key={config.name ?? idx} config={config} />)
              )}
            </div>
          </Card>
        </div>

        <Card className="bg-surface p-0">
          <CardHeader className="mb-0 border-b border-border px-4 py-3">
            <div>
              <CardTitle>Recent Activity</CardTitle>
              <CardDescription>Latest events across all monitored configs.</CardDescription>
            </div>
            <Badge variant="neutral">{events.length} event{events.length === 1 ? '' : 's'}</Badge>
          </CardHeader>
          <div className="max-h-[36rem] overflow-y-auto p-3">
            {query.isLoading ? (
              <div className="space-y-2">
                {Array.from({ length: 5 }, (_, i) => (
                  <div key={i} className="h-10 animate-pulse rounded-lg bg-panel/60" />
                ))}
              </div>
            ) : events.length === 0 ? (
              <p className="py-8 text-center text-xs text-dim">
                <Activity className="mx-auto mb-2 h-6 w-6" />
                No recent events.
              </p>
            ) : (
              <ul className="space-y-1.5">
                {events.map((event, idx) => (
                  <li
                    key={event.event_key ?? idx}
                    className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-lg border border-border bg-panel px-3 py-2"
                  >
                    <Badge variant={kindVariant(event.kind ?? '')}>{event.kind ?? 'unknown'}</Badge>
                    <span className="font-mono text-xs text-status-info">
                      {event.config_id ? `${event.config_id}:` : ''}#{event.issue_id ?? '?'}
                    </span>
                    <span className="ml-auto shrink-0 font-mono text-2xs text-dim">
                      {relTime(event.created_at)}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </Card>
      </div>
    </section>
  )
}
