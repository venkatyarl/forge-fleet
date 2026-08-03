'use client'

import { useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { formatDistanceToNowStrict } from 'date-fns'
import { ArrowUpFromLine, ListTodo, RefreshCw } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { StatusBadge } from '@/components/ui/status-badge'
import { getJson, postJson } from '@/lib/api'
import { cn } from '@/lib/utils'

/* GET /api/fleet/deferred → { tasks: [...] } (observed live shape below).
   POST /api/fleet/deferred/{id}/promote may answer 503
   {"error":{"message":"gateway authentication is not configured"}} on
   deployments without gateway auth — surfaced inline per row. */

type DeferredTask = {
  id?: string
  title?: string
  kind?: string
  status?: string
  created_at?: string | null
  attempts?: number
  max_attempts?: number
  last_error?: string | null
  preferred_node?: string | null
  trigger_type?: string | null
  payload?: { command?: string; summary?: string } | null
}

type DeferredResponse = { tasks?: DeferredTask[] }

type PromoteFeedback = { ok: boolean; message: string }

function relTime(value?: string | null): string {
  if (!value) return '—'
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return '—'
  return formatDistanceToNowStrict(date, { addSuffix: true })
}

function PromoteButton({ task }: { task: DeferredTask }) {
  const queryClient = useQueryClient()
  const [pending, setPending] = useState(false)
  const [feedback, setFeedback] = useState<PromoteFeedback | null>(null)

  const status = (task.status ?? '').toLowerCase()
  const promotable = task.id != null && !['completed', 'done', 'running', 'promoted'].includes(status)

  const promote = async () => {
    if (!task.id) return
    setPending(true)
    setFeedback(null)
    try {
      await postJson(`/api/fleet/deferred/${encodeURIComponent(task.id)}/promote`, {})
      setFeedback({ ok: true, message: 'promoted' })
      void queryClient.invalidateQueries({ queryKey: ['fleet', 'deferred'] })
    } catch (err) {
      const raw = err instanceof Error ? err.message : 'promotion failed'
      setFeedback({
        ok: false,
        message: /503|auth/i.test(raw)
          ? 'Promotion blocked by gateway auth policy'
          : raw,
      })
    } finally {
      setPending(false)
    }
  }

  return (
    <div className="flex flex-col items-end gap-1">
      <Button
        type="button"
        variant="outline"
        size="sm"
        onClick={() => void promote()}
        disabled={pending || !promotable}
        title={promotable ? 'Promote this task for immediate scheduling' : `cannot promote a ${status || 'unknown'} task`}
      >
        <ArrowUpFromLine className="h-3.5 w-3.5" />
        {pending ? 'Promoting…' : 'Promote'}
      </Button>
      {feedback ? (
        <span
          className={cn(
            'max-w-56 text-right font-mono text-2xs',
            feedback.ok ? 'text-status-ok' : 'text-status-crit'
          )}
        >
          {feedback.message}
        </span>
      ) : null}
    </div>
  )
}

export default function DeferredQueuePage() {
  const query = useQuery({
    queryKey: ['fleet', 'deferred'],
    refetchInterval: 15_000,
    retry: 1,
    queryFn: async () => {
      const payload = await getJson<DeferredResponse>('/api/fleet/deferred')
      return Array.isArray(payload?.tasks) ? payload.tasks : []
    },
  })

  const tasks = query.data ?? []
  const pendingCount = tasks.filter(
    (task) => !['completed', 'done'].includes((task.status ?? '').toLowerCase())
  ).length

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-xl font-semibold text-foreground">Deferred Queue</h2>
            {query.isLoading ? (
              <Badge variant="info">loading</Badge>
            ) : query.isError ? (
              <Badge variant="crit">unavailable</Badge>
            ) : (
              <Badge variant="ok">live</Badge>
            )}
            <Badge variant="neutral">{pendingCount} pending</Badge>
          </div>
          <p className="mt-1 text-sm text-muted">
            Tasks held back from scheduling — promote one to run it now. Refreshed every 15s.
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
          Error: {query.error instanceof Error ? query.error.message : 'failed to load deferred tasks'}
        </div>
      ) : null}

      <Card className="overflow-hidden bg-surface p-0">
        <CardHeader className="mb-0 border-b border-border px-4 py-3">
          <div>
            <CardTitle>Deferred Tasks</CardTitle>
            <CardDescription>Title, kind, status, target node, attempts, and age.</CardDescription>
          </div>
          <Badge variant="neutral">{tasks.length} task{tasks.length === 1 ? '' : 's'}</Badge>
        </CardHeader>
        <div className="overflow-x-auto">
          <table className="min-w-full text-left text-sm">
            <thead className="border-b border-border bg-elevated text-xs uppercase text-dim">
              <tr>
                <th className="px-4 py-2 font-medium">Task</th>
                <th className="px-4 py-2 font-medium">Kind</th>
                <th className="px-4 py-2 font-medium">Status</th>
                <th className="px-4 py-2 font-medium">Node</th>
                <th className="px-4 py-2 font-medium">Attempts</th>
                <th className="px-4 py-2 font-medium">Age</th>
                <th className="px-4 py-2 text-right font-medium">Action</th>
              </tr>
            </thead>
            <tbody>
              {query.isLoading ? (
                Array.from({ length: 4 }, (_, i) => (
                  <tr key={i} className="border-t border-border">
                    <td colSpan={7} className="px-4 py-3">
                      <div className="h-4 animate-pulse rounded bg-panel/60" />
                    </td>
                  </tr>
                ))
              ) : tasks.length === 0 ? (
                <tr>
                  <td className="px-4 py-8 text-center text-sm text-dim" colSpan={7}>
                    <ListTodo className="mx-auto mb-2 h-6 w-6" />
                    The deferred queue is empty.
                  </td>
                </tr>
              ) : (
                tasks.map((task, idx) => (
                  <tr
                    key={task.id ?? idx}
                    className="border-t border-border text-muted transition hover:bg-panel hover:text-foreground"
                  >
                    <td className="max-w-md px-4 py-3">
                      <div className="truncate font-mono text-xs text-status-info" title={task.title ?? ''}>
                        {task.title ?? 'untitled'}
                      </div>
                      {task.payload?.summary ? (
                        <div className="mt-0.5 truncate text-2xs text-dim" title={task.payload.summary}>
                          {task.payload.summary}
                        </div>
                      ) : null}
                      {task.last_error ? (
                        <div className="mt-0.5 truncate font-mono text-2xs text-status-crit" title={task.last_error}>
                          {task.last_error}
                        </div>
                      ) : null}
                    </td>
                    <td className="px-4 py-3">
                      {task.kind ? <Badge variant="neutral">{task.kind}</Badge> : '—'}
                    </td>
                    <td className="px-4 py-3">
                      <StatusBadge status={task.status ?? 'unknown'} />
                    </td>
                    <td className="px-4 py-3">
                      {task.preferred_node ? <Badge variant="neutral">{task.preferred_node}</Badge> : '—'}
                    </td>
                    <td className="px-4 py-3 text-xs">
                      {task.attempts ?? 0}
                      {task.max_attempts != null ? ` / ${task.max_attempts}` : ''}
                    </td>
                    <td className="whitespace-nowrap px-4 py-3 text-xs text-dim">
                      {relTime(task.created_at)}
                    </td>
                    <td className="px-4 py-3 text-right">
                      <PromoteButton task={task} />
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
