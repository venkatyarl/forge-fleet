import { useCallback, useEffect, useState } from 'react'
import { Badge, type BadgeProps } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { getJson, postJson } from '../lib/api'
import { cn } from '../lib/utils'

/* -- types -------------------------------------------------- */

type NodeVersion = {
  name: string
  version: string
  status?: string
  stage?: string
}

type RolloutInfo = {
  id?: string
  version?: string
  stage?: string // canary | rolling | complete | paused | aborted
  progress?: number
  startedAt?: string
  completedAt?: string
  nodes?: NodeVersion[]
}

type UpdateCheck = {
  available: boolean
  latestVersion?: string
  currentVersion?: string
  releaseNotes?: string
  url?: string
}

type UpdateHistory = {
  id?: string
  version: string
  date: string
  status: string
  duration?: string
}

type UpdateResponse = {
  currentVersion?: string
  rollout?: RolloutInfo
  history?: UpdateHistory[]
  nodeVersions?: NodeVersion[]
  [key: string]: unknown
}

/* -- helpers ------------------------------------------------ */

const STAGE_TEXT: Record<string, string> = {
  canary: 'text-status-warn',
  rolling: 'text-status-info',
  complete: 'text-status-ok',
  paused: 'text-status-warn',
  aborted: 'text-status-crit',
}

function statusVariant(status: string): BadgeProps['variant'] {
  const s = status.toLowerCase()
  if (['success', 'complete', 'completed', 'healthy', 'ready', 'active'].includes(s)) return 'ok'
  if (['failed', 'failure', 'aborted', 'error', 'critical', 'down'].includes(s)) return 'crit'
  if (['canary', 'paused', 'warning', 'degraded'].includes(s)) return 'warn'
  if (['rolling', 'running', 'pending', 'queued', 'checking', 'info'].includes(s)) return 'info'
  return 'neutral'
}

function formatDate(value: string) {
  try {
    return new Date(value).toLocaleString()
  } catch {
    return value
  }
}

/* -- component --------------------------------------------- */

export function Updates() {
  const [data, setData] = useState<UpdateResponse | null>(null)
  const [checking, setChecking] = useState(false)
  const [checkResult, setCheckResult] = useState<UpdateCheck | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [actionMsg, setActionMsg] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const resp = await getJson<UpdateResponse>('/api/update/status').catch(() => null)
      setData(resp)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load update status')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => { load() }, [load])

  const checkForUpdates = useCallback(async () => {
    setChecking(true)
    setCheckResult(null)
    try {
      const result = await getJson<UpdateCheck>('/api/update/check')
      setCheckResult(result)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Check failed')
    } finally {
      setChecking(false)
    }
  }, [])

  const rolloutAction = useCallback(async (action: 'pause' | 'resume' | 'abort') => {
    try {
      setActionMsg(null)
      await postJson(`/api/update/${action}`, {})
      setActionMsg(`Rollout ${action}d successfully`)
      load()
    } catch (err) {
      setError(err instanceof Error ? err.message : `Failed to ${action} rollout`)
    }
  }, [load])

  const currentVersion = data?.currentVersion ?? '-'
  const rollout = data?.rollout
  const rolloutStage = rollout?.stage?.toLowerCase() ?? ''
  const progress = Math.min(100, Math.max(0, rollout?.progress ?? 0))
  const hasRolloutState = Boolean(
    rollout && (rollout.id || rollout.version || rollout.progress != null || rolloutStage),
  )
  const hasActiveRollout = rolloutStage === 'canary' || rolloutStage === 'rolling' || rolloutStage === 'paused'
  const canPause = rolloutStage === 'canary' || rolloutStage === 'rolling'
  const canResume = rolloutStage === 'paused'
  const history = data?.history ?? []
  const nodeVersions = data?.nodeVersions ?? rollout?.nodes ?? []

  return (
    <section className="min-h-full space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <h2 className="text-xl font-semibold text-foreground">Update Rollout</h2>
          <p className="mt-1 text-sm text-muted">
            Fleet version status, rollout progress, and update history.
          </p>
        </div>
        <div className="flex items-center gap-2">
          {loading ? <Badge variant="info">loading</Badge> : <Badge variant="ok">ready</Badge>}
          <Button onClick={() => void load()} type="button" variant="outline" disabled={loading}>
            {loading ? 'Loading' : 'Refresh'}
          </Button>
        </div>
      </div>

      {error ? (
        <Card className="border-border-subtle bg-surface">
          <CardHeader>
            <div>
              <CardTitle className="text-status-crit">Update Error</CardTitle>
              <CardDescription>The latest update operation did not complete.</CardDescription>
            </div>
            <StatusBadge status="error">error</StatusBadge>
          </CardHeader>
          <p className="text-sm text-status-crit">{error}</p>
        </Card>
      ) : null}

      {actionMsg ? (
        <Card className="border-border-subtle bg-surface">
          <CardHeader>
            <div>
              <CardTitle className="text-status-ok">Rollout Action</CardTitle>
              <CardDescription>The fleet accepted the requested rollout action.</CardDescription>
            </div>
            <StatusBadge status="success">success</StatusBadge>
          </CardHeader>
          <p className="text-sm text-muted">{actionMsg}</p>
        </Card>
      ) : null}

      <div className="grid gap-3 md:grid-cols-3">
        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Current Version</CardTitle>
              <CardDescription>Version reported by the update controller.</CardDescription>
            </div>
            <Badge variant="default">fleet</Badge>
          </CardHeader>
          <p className="text-3xl font-semibold text-primary">{currentVersion}</p>
        </Card>

        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Update Check</CardTitle>
              <CardDescription>Query the release endpoint for newer builds.</CardDescription>
            </div>
            {checking ? <StatusBadge status="running">checking</StatusBadge> : null}
          </CardHeader>
          <div className="space-y-3">
            <Button onClick={checkForUpdates} type="button" disabled={checking}>
              {checking ? 'Checking' : 'Check Now'}
            </Button>
            {checkResult ? (
              <div className="rounded-lg border border-border bg-surface p-3 text-sm">
                {checkResult.available ? (
                  <div className="space-y-2">
                    <div className="flex flex-wrap items-center gap-2">
                      <Badge variant="ok">available</Badge>
                      <span className="font-medium text-status-ok">
                        {checkResult.latestVersion ?? 'new version'}
                      </span>
                    </div>
                    {checkResult.releaseNotes ? (
                      <p className="text-xs leading-relaxed text-muted">{checkResult.releaseNotes}</p>
                    ) : null}
                  </div>
                ) : (
                  <div className="flex flex-wrap items-center gap-2">
                    <Badge variant="neutral">current</Badge>
                    <span className="text-muted">You're on the latest version.</span>
                  </div>
                )}
              </div>
            ) : null}
          </div>
        </Card>

        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Rollout Status</CardTitle>
              <CardDescription>Current deployment stage and completion.</CardDescription>
            </div>
            {hasRolloutState ? (
              <Badge variant={statusVariant(rollout?.stage ?? 'unknown')}>
                {rollout?.stage ?? 'unknown'}
              </Badge>
            ) : (
              <Badge variant="neutral">idle</Badge>
            )}
          </CardHeader>
          {hasRolloutState ? (
            <div className="space-y-3">
              <div className="flex items-baseline gap-2">
                <span className={cn('text-2xl font-semibold', STAGE_TEXT[rolloutStage] ?? 'text-muted')}>
                  {rollout?.stage ?? 'unknown'}
                </span>
                {rollout?.version ? (
                  <span className="text-sm text-muted">v{rollout.version}</span>
                ) : null}
              </div>
              {rollout?.progress != null ? (
                <div className="space-y-2">
                  <div className="h-2 w-full overflow-hidden rounded-full bg-elevated">
                    <div
                      className="h-full rounded-full bg-primary transition-all"
                      style={{ width: `${progress}%` }}
                    />
                  </div>
                  <p className="text-xs text-dim">{rollout.progress}% complete</p>
                </div>
              ) : null}
            </div>
          ) : (
            <p className="text-sm text-muted">No active rollout.</p>
          )}
        </Card>
      </div>

      {hasActiveRollout ? (
        <Card className="bg-surface">
          <CardHeader>
            <div>
              <CardTitle>Rollout Controls</CardTitle>
              <CardDescription>Pause, resume, or abort the active deployment.</CardDescription>
            </div>
            <StatusBadge status={rolloutStage}>{rolloutStage}</StatusBadge>
          </CardHeader>
          <div className="flex flex-wrap gap-2">
            <Button
              onClick={() => void rolloutAction('pause')}
              type="button"
              variant="outline"
              disabled={!canPause}
              className="border-border-subtle text-status-warn hover:bg-elevated"
            >
              Pause
            </Button>
            <Button
              onClick={() => void rolloutAction('resume')}
              type="button"
              variant="outline"
              disabled={!canResume}
              className="border-border-subtle text-status-ok hover:bg-elevated"
            >
              Resume
            </Button>
            <Button
              onClick={() => void rolloutAction('abort')}
              type="button"
              variant="outline"
              disabled={!hasActiveRollout}
              className="border-border-subtle text-status-crit hover:bg-elevated"
            >
              Abort
            </Button>
          </div>
        </Card>
      ) : null}

      {nodeVersions.length > 0 ? (
        <Card className="bg-surface">
          <CardHeader>
            <div>
              <CardTitle>Per-Node Versions</CardTitle>
              <CardDescription>Reported version and rollout state by node.</CardDescription>
            </div>
            <Badge variant="neutral">{nodeVersions.length} nodes</Badge>
          </CardHeader>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
            {nodeVersions.map((nv) => {
              const status = nv.status ?? nv.stage ?? 'unknown'
              return (
                <div
                  key={nv.name}
                  className="rounded-lg border border-border bg-panel px-4 py-3"
                >
                  <div className="flex items-center justify-between gap-2">
                    <span className="min-w-0 truncate font-medium text-foreground">{nv.name}</span>
                    <Badge variant={statusVariant(status)}>{status}</Badge>
                  </div>
                  <p className="mt-2 text-sm font-medium text-primary">{nv.version}</p>
                </div>
              )
            })}
          </div>
        </Card>
      ) : null}

      <Card className="overflow-hidden bg-surface p-0">
        <CardHeader className="mb-0 border-b border-border px-4 py-3">
          <div>
            <CardTitle>Rollout History</CardTitle>
            <CardDescription>Last recorded update attempts.</CardDescription>
          </div>
          <Badge variant="neutral">{history.length} entries</Badge>
        </CardHeader>
        {history.length === 0 ? (
          <p className="px-4 py-8 text-center text-sm text-dim">No rollout history available.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="min-w-full text-left text-sm">
              <thead className="border-b border-border bg-elevated text-xs uppercase text-dim">
                <tr>
                  <th className="px-4 py-2 font-medium">Version</th>
                  <th className="px-4 py-2 font-medium">Date</th>
                  <th className="px-4 py-2 font-medium">Status</th>
                  <th className="px-4 py-2 font-medium">Duration</th>
                </tr>
              </thead>
              <tbody>
                {history.slice(0, 10).map((h, idx) => (
                  <tr
                    key={h.id ?? `${h.version}-${idx}`}
                    className="border-b border-border/70 transition-colors hover:bg-panel"
                  >
                    <td className="px-4 py-3 font-medium text-primary">{h.version}</td>
                    <td className="px-4 py-3 text-xs text-muted">{formatDate(h.date)}</td>
                    <td className="px-4 py-3">
                      <Badge variant={statusVariant(h.status)}>{h.status}</Badge>
                    </td>
                    <td className="px-4 py-3 text-muted">{h.duration ?? '-'}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </section>
  )
}
