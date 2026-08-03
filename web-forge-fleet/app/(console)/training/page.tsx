'use client'

import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { formatDistanceToNowStrict } from 'date-fns'
import { ChevronDown, ChevronRight, GraduationCap, RefreshCw } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { getJson } from '@/lib/api'
import { cn } from '@/lib/utils'

/* GET /api/training/jobs → { jobs: [...] }. loss_curve / params are jsonb and
   may be empty or shaped differently per job — everything is read defensively. */

type TrainingJob = {
  id?: string
  name?: string
  base_model_id?: string | null
  training_type?: string
  status?: string
  created_at?: string | null
  started_at?: string | null
  completed_at?: string | null
  created_by?: string | null
  error_message?: string | null
  result_model_id?: string | null
  loss_curve?: unknown
  params?: unknown
}

type JobsResponse = { jobs?: TrainingJob[] }

function statusVariant(status: string): 'neutral' | 'info' | 'ok' | 'crit' {
  const s = status.toLowerCase()
  if (s === 'running' || s === 'in_progress' || s === 'training') return 'info'
  if (s === 'completed' || s === 'done' || s === 'success' || s === 'succeeded') return 'ok'
  if (s === 'failed' || s === 'error' || s === 'cancelled' || s === 'canceled') return 'crit'
  return 'neutral' // queued and anything unrecognized
}

function relTime(value?: string | null): string {
  if (!value) return '—'
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return '—'
  return formatDistanceToNowStrict(date, { addSuffix: true })
}

/** loss_curve is a jsonb array: tolerate numbers or {step, loss}-ish objects. */
function lossPoints(curve: unknown): number[] {
  if (!Array.isArray(curve)) return []
  const points: number[] = []
  for (const point of curve) {
    if (typeof point === 'number' && Number.isFinite(point)) {
      points.push(point)
    } else if (point && typeof point === 'object') {
      const obj = point as Record<string, unknown>
      const v = obj.loss ?? obj.value ?? obj.y
      if (typeof v === 'number' && Number.isFinite(v)) points.push(v)
    }
  }
  return points
}

function LossSparkline({ points }: { points: number[] }) {
  if (points.length === 0) return null
  const w = 140
  const h = 36
  const pad = 2
  if (points.length === 1) {
    return (
      <svg width={w} height={h} className="text-status-info" aria-label="single loss point">
        <circle cx={w / 2} cy={h / 2} r={3} fill="currentColor" />
      </svg>
    )
  }
  const min = Math.min(...points)
  const max = Math.max(...points)
  const span = max - min || 1
  const coords = points
    .map((v, i) => {
      const x = pad + (i / (points.length - 1)) * (w - pad * 2)
      const y = pad + (1 - (v - min) / span) * (h - pad * 2)
      return `${x.toFixed(1)},${y.toFixed(1)}`
    })
    .join(' ')
  return (
    <div className="flex items-center gap-3">
      <svg width={w} height={h} className="text-status-info" aria-label="loss curve sparkline">
        <polyline points={coords} fill="none" stroke="currentColor" strokeWidth="1.5" />
      </svg>
      <span className="font-mono text-2xs text-dim">
        loss {points[points.length - 1]?.toPrecision(4)} · {points.length} pts
      </span>
    </div>
  )
}

function JobRow({ job }: { job: TrainingJob }) {
  const [expanded, setExpanded] = useState(false)
  const status = job.status ?? 'unknown'
  const points = lossPoints(job.loss_curve)
  const hasParams = job.params != null && Object.keys(job.params as object).length > 0
  const expandable = hasParams || job.error_message != null || job.result_model_id != null || points.length > 0

  return (
    <>
      <tr
        className={cn(
          'border-t border-border text-muted transition',
          expandable && 'cursor-pointer hover:bg-panel hover:text-foreground'
        )}
        onClick={() => expandable && setExpanded((v) => !v)}
      >
        <td className="px-4 py-3">
          <div className="flex items-center gap-1.5">
            {expandable ? (
              expanded ? (
                <ChevronDown className="h-3.5 w-3.5 shrink-0 text-dim" />
              ) : (
                <ChevronRight className="h-3.5 w-3.5 shrink-0 text-dim" />
              )
            ) : (
              <span className="w-3.5" />
            )}
            <span className="font-mono text-xs text-status-info">{job.name ?? 'unnamed'}</span>
          </div>
        </td>
        <td className="px-4 py-3">
          {job.training_type ? <Badge variant="neutral">{job.training_type}</Badge> : '—'}
        </td>
        <td className="px-4 py-3">
          <Badge variant={statusVariant(status)}>{status}</Badge>
        </td>
        <td className="px-4 py-3 font-mono text-xs text-muted">
          {job.base_model_id ?? '—'}
        </td>
        <td className="px-4 py-3 text-xs text-muted">{job.created_by ?? '—'}</td>
        <td className="whitespace-nowrap px-4 py-3 text-xs text-dim">{relTime(job.created_at)}</td>
      </tr>
      {expanded ? (
        <tr className="border-t border-border bg-panel/40">
          <td colSpan={6} className="px-4 py-3">
            <div className="space-y-3 text-xs">
              <div className="flex flex-wrap gap-x-6 gap-y-1 text-dim">
                <span>
                  started: <span className="text-muted">{relTime(job.started_at)}</span>
                </span>
                <span>
                  completed: <span className="text-muted">{relTime(job.completed_at)}</span>
                </span>
                {job.result_model_id ? (
                  <span>
                    result model:{' '}
                    <span className="font-mono text-status-ok">{job.result_model_id}</span>
                  </span>
                ) : null}
                <span className="font-mono text-2xs">id: {job.id ?? '—'}</span>
              </div>
              {job.error_message ? (
                <pre className="overflow-x-auto rounded-lg border border-status-crit bg-background p-3 font-mono text-2xs text-status-crit">
                  {job.error_message}
                </pre>
              ) : null}
              {points.length > 0 ? <LossSparkline points={points} /> : null}
              {hasParams ? (
                <pre className="max-h-64 overflow-auto rounded-lg border border-border bg-background p-3 font-mono text-2xs text-muted">
                  {JSON.stringify(job.params, null, 2)}
                </pre>
              ) : null}
            </div>
          </td>
        </tr>
      ) : null}
    </>
  )
}

export default function TrainingPage() {
  const query = useQuery({
    queryKey: ['training', 'jobs'],
    refetchInterval: 15_000,
    retry: 1,
    queryFn: async () => {
      const payload = await getJson<JobsResponse>('/api/training/jobs')
      return Array.isArray(payload?.jobs) ? payload.jobs : []
    },
  })

  const jobs = query.data ?? []
  const running = jobs.filter((j) => (j.status ?? '').toLowerCase() === 'running').length

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-xl font-semibold text-foreground">Training Jobs</h2>
            {query.isLoading ? (
              <Badge variant="info">loading</Badge>
            ) : query.isError ? (
              <Badge variant="crit">unavailable</Badge>
            ) : (
              <Badge variant="ok">live</Badge>
            )}
            {running > 0 ? <Badge variant="info">{running} running</Badge> : null}
          </div>
          <p className="mt-1 text-sm text-muted">
            Queued and completed ff-LLM training runs, refreshed every 15s.
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
          Error: {query.error instanceof Error ? query.error.message : 'failed to load training jobs'}
        </div>
      ) : null}

      <Card className="overflow-hidden bg-surface p-0">
        <CardHeader className="mb-0 border-b border-border px-4 py-3">
          <div>
            <CardTitle>Jobs</CardTitle>
            <CardDescription>Click a row to expand params, errors, and loss curves.</CardDescription>
          </div>
          <Badge variant="neutral">{jobs.length} job{jobs.length === 1 ? '' : 's'}</Badge>
        </CardHeader>
        <div className="overflow-x-auto">
          <table className="min-w-full text-left text-sm">
            <thead className="border-b border-border bg-elevated text-xs uppercase text-dim">
              <tr>
                <th className="px-4 py-2 font-medium">Name</th>
                <th className="px-4 py-2 font-medium">Type</th>
                <th className="px-4 py-2 font-medium">Status</th>
                <th className="px-4 py-2 font-medium">Base Model</th>
                <th className="px-4 py-2 font-medium">Created By</th>
                <th className="px-4 py-2 font-medium">Created</th>
              </tr>
            </thead>
            <tbody>
              {query.isLoading ? (
                Array.from({ length: 4 }, (_, i) => (
                  <tr key={i} className="border-t border-border">
                    <td colSpan={6} className="px-4 py-3">
                      <div className="h-4 animate-pulse rounded bg-panel/60" />
                    </td>
                  </tr>
                ))
              ) : jobs.length === 0 ? (
                <tr>
                  <td className="px-4 py-8 text-center text-sm text-dim" colSpan={6}>
                    <GraduationCap className="mx-auto mb-2 h-6 w-6" />
                    No training jobs recorded yet.
                  </td>
                </tr>
              ) : (
                jobs.map((job, idx) => <JobRow key={job.id ?? idx} job={job} />)
              )}
            </tbody>
          </table>
        </div>
      </Card>
    </section>
  )
}
