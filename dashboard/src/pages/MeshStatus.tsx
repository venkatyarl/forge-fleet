import { useEffect, useMemo, useState, type ReactNode } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { getJson } from '../lib/api'
import { cn } from '../lib/utils'

interface MeshRow {
  src_node: string
  dst_node: string
  status: string
  last_checked: string | null
  last_error: string | null
  attempts: number
}

interface MeshResp {
  matrix: MeshRow[]
  count: number
}

interface RetryTask {
  id: string
  title: string
  status: string
  attempts: number
  max_attempts: number
  last_error?: string | null
}

type Tone = 'ok' | 'warn' | 'crit' | 'info' | 'neutral'

function statusTone(status: string | undefined): Tone {
  switch (status) {
    case 'ok':
      return 'ok'
    case 'failed':
      return 'crit'
    case 'pending':
      return 'warn'
    default:
      return 'neutral'
  }
}

function cellColor(status: string | undefined): string {
  return toneColor(statusTone(status))
}

function toneColor(tone: Tone): string {
  switch (tone) {
    case 'ok':
      return 'text-status-ok'
    case 'crit':
      return 'text-status-crit'
    case 'warn':
      return 'text-status-warn'
    case 'info':
      return 'text-status-info'
    default:
      return 'text-dim'
  }
}

function cellIcon(status: string | undefined): string {
  switch (status) {
    case 'ok':
      return '✓'
    case 'failed':
      return '✗'
    case 'pending':
      return '…'
    default:
      return '—'
  }
}

function toneLabel(tone: Tone): string {
  switch (tone) {
    case 'ok':
      return 'healthy'
    case 'crit':
      return 'failed'
    case 'warn':
      return 'degraded'
    case 'info':
      return 'checking'
    default:
      return 'unknown'
  }
}

export function MeshStatus() {
  const [rows, setRows] = useState<MeshRow[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [selected, setSelected] = useState<{ src: string; dst: string } | null>(null)
  const [retryTask, setRetryTask] = useState<RetryTask | null>(null)
  const [retrying, setRetrying] = useState(false)

  useEffect(() => {
    let cancelled = false
    async function load() {
      try {
        const d = await getJson<MeshResp>('/api/fleet/mesh-check')
        if (cancelled) return
        setRows(d.matrix || [])
        setError(null)
        setLoading(false)
      } catch (e) {
        if (cancelled) return
        setError(e instanceof Error ? e.message : String(e))
        setLoading(false)
      }
    }
    load()
    const t = setInterval(load, 30000)
    return () => {
      cancelled = true
      clearInterval(t)
    }
  }, [])

  const nodes = useMemo(() => {
    const s = new Set<string>()
    for (const r of rows) {
      s.add(r.src_node)
      s.add(r.dst_node)
    }
    return Array.from(s).sort()
  }, [rows])

  const lookup = useMemo(() => {
    const m = new Map<string, MeshRow>()
    for (const r of rows) m.set(`${r.src_node}→${r.dst_node}`, r)
    return m
  }, [rows])

  const summary = useMemo(() => {
    const counts = { ok: 0, failed: 0, pending: 0, unknown: 0 }
    for (const row of rows) {
      if (row.status === 'ok') counts.ok += 1
      else if (row.status === 'failed') counts.failed += 1
      else if (row.status === 'pending') counts.pending += 1
      else counts.unknown += 1
    }
    return counts
  }, [rows])

  const meshTone: Tone =
    summary.failed > 0 ? 'crit' : summary.pending > 0 || summary.unknown > 0 ? 'warn' : 'ok'
  const selRow = selected ? lookup.get(`${selected.src}→${selected.dst}`) : null

  useEffect(() => {
    if (!selected || !selRow || selRow.status === 'ok') {
      setRetryTask(null)
      return
    }
    const qs = new URLSearchParams({ status: 'pending', kind: 'mesh_retry' })
    getJson<{ tasks: (RetryTask & { payload?: { src?: string; dst?: string } })[] }>(
      `/api/fleet/deferred?${qs.toString()}`
    )
      .then((r) => {
        const match = r.tasks.find(
          (t) => t.payload?.src === selected.src && t.payload?.dst === selected.dst
        )
        setRetryTask(match || null)
      })
      .catch(() => setRetryTask(null))
  }, [selected, selRow])

  const runRetry = async () => {
    if (!retryTask) return
    setRetrying(true)
    try {
      await fetch(`/api/fleet/deferred/${retryTask.id}/promote`, { method: 'POST' })
      setRetryTask(null)
    } finally {
      setRetrying(false)
    }
  }

  if (loading) {
    return (
      <section className="min-h-full bg-background p-6 text-foreground">
        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Mesh SSH reachability</CardTitle>
              <CardDescription>Loading mesh matrix...</CardDescription>
            </div>
            <Badge variant="info">syncing</Badge>
          </CardHeader>
          <div className="grid gap-3 sm:grid-cols-3">
            {[1, 2, 3].map((item) => (
              <div key={item} className="h-16 rounded-lg border border-border bg-surface" />
            ))}
          </div>
        </Card>
      </section>
    )
  }

  if (error) {
    return (
      <section className="min-h-full bg-background p-6 text-foreground">
        <Card className="border-status-crit bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Mesh SSH reachability</CardTitle>
              <CardDescription>Fleet mesh check failed</CardDescription>
            </div>
            <Badge variant="crit">error</Badge>
          </CardHeader>
          <div className="text-sm text-status-crit">{error}</div>
        </Card>
      </section>
    )
  }

  if (nodes.length === 0) {
    return (
      <section className="min-h-full bg-background p-6 text-foreground">
        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Mesh SSH reachability</CardTitle>
              <CardDescription>No mesh status rows have been reported yet</CardDescription>
            </div>
            <Badge variant="neutral">empty</Badge>
          </CardHeader>
          <p className="text-sm text-muted">
            Run <code className="rounded-sm bg-elevated px-1.5 py-0.5 text-foreground">ff fleet ssh-mesh-check</code>{' '}
            on taylor to populate the matrix.
          </p>
        </Card>
      </section>
    )
  }

  return (
    <section className="min-h-full space-y-6 bg-background p-6 text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Mesh SSH reachability</h1>
            <StatusBadge status={toneLabel(meshTone)}>{toneLabel(meshTone)}</StatusBadge>
          </div>
          <p className="mt-1 text-sm text-dim">
            Rows are source nodes, columns are destination nodes. Select a cell for task detail.
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Badge variant="neutral">{nodes.length} nodes</Badge>
          <Badge variant="neutral">{rows.length} links</Badge>
        </div>
      </div>

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <Metric label="Reachable" value={summary.ok} tone="ok" />
        <Metric label="Failed" value={summary.failed} tone="crit" />
        <Metric label="Pending" value={summary.pending} tone="warn" />
        <Metric label="Unknown" value={summary.unknown} tone="neutral" />
      </div>

      <Card className="bg-surface p-0">
        <CardHeader className="mb-0 border-b border-border p-4">
          <div>
            <CardTitle>Reachability Matrix</CardTitle>
            <CardDescription>
              <span className="text-status-ok">✓ ok</span>
              <span className="mx-2 text-border-subtle">/</span>
              <span className="text-status-crit">✗ failed</span>
              <span className="mx-2 text-border-subtle">/</span>
              <span className="text-status-warn">… pending</span>
            </CardDescription>
          </div>
          <Badge variant={meshTone}>{toneLabel(meshTone)}</Badge>
        </CardHeader>

        <div className="overflow-x-auto">
          <table className="min-w-full border-collapse text-sm">
            <thead>
              <tr className="border-b border-border bg-panel">
                <th className="sticky left-0 z-10 bg-panel px-3 py-2 text-left text-xs font-semibold uppercase text-dim">
                  src \ dst
                </th>
                {nodes.map((node) => (
                  <th
                    key={node}
                    className="px-3 py-2 text-left text-xs font-semibold uppercase text-dim"
                  >
                    <span className="block max-w-32 truncate">{node}</span>
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {nodes.map((src) => (
                <tr key={src} className="border-b border-border hover:bg-panel">
                  <td className="sticky left-0 z-10 bg-surface px-3 py-2 font-mono text-xs text-muted">
                    {src}
                  </td>
                  {nodes.map((dst) => {
                    if (src === dst) {
                      return (
                        <td key={dst} className="px-3 py-2 text-center text-dim">
                          ·
                        </td>
                      )
                    }
                    const row = lookup.get(`${src}→${dst}`)
                    const isSelected = selected?.src === src && selected?.dst === dst
                    return (
                      <td key={dst} className="px-3 py-2 text-center">
                        <button
                          type="button"
                          onClick={() => setSelected({ src, dst })}
                          className={cn(
                            'inline-flex h-8 w-9 items-center justify-center rounded-lg border font-mono text-xs transition',
                            cellColor(row?.status),
                            isSelected
                              ? 'border-primary/50 bg-primary-subtle'
                              : 'border-transparent hover:border-border-subtle hover:bg-elevated'
                          )}
                          aria-label={`${src} to ${dst}: ${row?.status ?? 'unknown'}`}
                        >
                          {cellIcon(row?.status)}
                        </button>
                      </td>
                    )
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Card>

      {selected && selRow && (
        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle className="font-mono text-base">
                {selected.src} → {selected.dst}
              </CardTitle>
              <CardDescription>Selected mesh edge detail</CardDescription>
            </div>
            <Badge variant={statusTone(selRow.status)}>{selRow.status}</Badge>
          </CardHeader>

          <dl className="grid gap-3 text-sm sm:grid-cols-2">
            <Detail label="status">
              <span className={cellColor(selRow.status)}>{selRow.status}</span>
            </Detail>
            <Detail label="last_checked">{selRow.last_checked || '—'}</Detail>
            <Detail label="attempts">{selRow.attempts}</Detail>
            <Detail label="last_error">
              <span className="break-all text-status-crit">{selRow.last_error || '—'}</span>
            </Detail>
          </dl>

          {retryTask && (
            <div className="mt-4 border-t border-border pt-4">
              <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
                <div>
                  <div className="flex flex-wrap items-center gap-2">
                    <StatusBadge status={retryTask.status}>{retryTask.status}</StatusBadge>
                    <Badge variant="warn">
                      attempts {retryTask.attempts}/{retryTask.max_attempts}
                    </Badge>
                  </div>
                  <p className="mt-2 text-sm text-muted">{retryTask.title}</p>
                  {retryTask.last_error ? (
                    <p className="mt-1 break-all text-xs text-status-crit">{retryTask.last_error}</p>
                  ) : null}
                </div>
                <Button onClick={runRetry} disabled={retrying}>
                  {retrying ? 'Promoting...' : 'Run retry now'}
                </Button>
              </div>
            </div>
          )}

          <div className="mt-4 flex justify-end border-t border-border pt-4">
            <Button variant="outline" size="sm" onClick={() => setSelected(null)}>
              Close
            </Button>
          </div>
        </Card>
      )}
    </section>
  )
}

function Metric({ label, value, tone }: { label: string; value: number; tone: Tone }) {
  return (
    <Card className="bg-panel">
      <div className="text-xs text-dim">{label}</div>
      <div className={cn('mt-1 text-2xl font-bold', toneColor(tone))}>{value}</div>
    </Card>
  )
}

function Detail({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs uppercase text-dim">{label}</dt>
      <dd className="mt-1 font-mono text-sm text-muted">{children}</dd>
    </div>
  )
}
