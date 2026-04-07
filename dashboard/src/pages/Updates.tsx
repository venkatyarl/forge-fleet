import { useCallback, useEffect, useState } from 'react'
import { getJson, postJson } from '../lib/api'

/* ── types ───────────────────────────────────────────────── */

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

/* ── helpers ─────────────────────────────────────────────── */

const STAGE_COLORS: Record<string, string> = {
  canary: 'text-amber-400',
  rolling: 'text-sky-400',
  complete: 'text-emerald-400',
  paused: 'text-orange-400',
  aborted: 'text-rose-400',
}

const STATUS_BADGES: Record<string, { bg: string; text: string }> = {
  success: { bg: 'bg-emerald-500/15', text: 'text-emerald-300' },
  complete: { bg: 'bg-emerald-500/15', text: 'text-emerald-300' },
  failed: { bg: 'bg-rose-500/15', text: 'text-rose-300' },
  aborted: { bg: 'bg-rose-500/15', text: 'text-rose-300' },
  rolling: { bg: 'bg-sky-500/15', text: 'text-sky-300' },
  canary: { bg: 'bg-amber-500/15', text: 'text-amber-300' },
  paused: { bg: 'bg-orange-500/15', text: 'text-orange-300' },
}

function badge(status: string) {
  const s = status.toLowerCase()
  return STATUS_BADGES[s] ?? { bg: 'bg-slate-500/15', text: 'text-slate-300' }
}

/* ── component ───────────────────────────────────────────── */

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

  const currentVersion = data?.currentVersion ?? '—'
  const rollout = data?.rollout
  const rolloutStage = rollout?.stage?.toLowerCase() ?? ''
  const hasRolloutState = Boolean(
    rollout && (rollout.id || rollout.version || rollout.progress != null || rolloutStage),
  )
  const hasActiveRollout = rolloutStage === 'canary' || rolloutStage === 'rolling' || rolloutStage === 'paused'
  const canPause = rolloutStage === 'canary' || rolloutStage === 'rolling'
  const canResume = rolloutStage === 'paused'
  const history = data?.history ?? []
  const nodeVersions = data?.nodeVersions ?? rollout?.nodes ?? []

  return (
    <section className="space-y-6">
      {/* header */}
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Update Rollout</h1>
          <p className="mt-1 text-sm text-slate-400">
            Manage fleet updates — version status, rollout progress, and history
          </p>
        </div>
        <button
          onClick={load}
          disabled={loading}
          className="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"
        >
          {loading ? 'Loading…' : '↻ Refresh'}
        </button>
      </div>

      {error && (
        <div className="rounded-xl border border-rose-500/30 bg-rose-500/10 px-4 py-3 text-sm text-rose-200">
          {error}
        </div>
      )}

      {actionMsg && (
        <div className="rounded-xl border border-emerald-500/30 bg-emerald-500/10 px-4 py-3 text-sm text-emerald-200">
          {actionMsg}
        </div>
      )}

      {/* current version + check */}
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        <div className="rounded-xl border border-slate-800 bg-slate-900/50 p-5">
          <p className="text-xs uppercase tracking-wider text-slate-500">Current Version</p>
          <p className="mt-1 text-3xl font-bold text-sky-300">{currentVersion}</p>
        </div>

        <div className="rounded-xl border border-slate-800 bg-slate-900/50 p-5">
          <p className="text-xs uppercase tracking-wider text-slate-500">Check for Updates</p>
          <button
            onClick={checkForUpdates}
            disabled={checking}
            className="mt-2 rounded-lg bg-indigo-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-indigo-500 disabled:opacity-50"
          >
            {checking ? 'Checking…' : '🔍 Check Now'}
          </button>
          {checkResult && (
            <div className="mt-3 text-sm">
              {checkResult.available ? (
                <div className="space-y-1">
                  <p className="font-medium text-emerald-400">
                    Update available: {checkResult.latestVersion}
                  </p>
                  {checkResult.releaseNotes && (
                    <p className="text-xs text-slate-400">{checkResult.releaseNotes}</p>
                  )}
                </div>
              ) : (
                <p className="text-slate-400">You're on the latest version</p>
              )}
            </div>
          )}
        </div>

        {/* rollout status */}
        <div className="rounded-xl border border-slate-800 bg-slate-900/50 p-5">
          <p className="text-xs uppercase tracking-wider text-slate-500">Rollout Status</p>
          {hasRolloutState ? (
            <div className="mt-2 space-y-2">
              <div className="flex items-center gap-2">
                <span className={`text-lg font-bold ${STAGE_COLORS[rollout?.stage ?? ''] ?? 'text-slate-300'}`}>
                  {rollout?.stage ?? 'unknown'}
                </span>
                {rollout?.version && (
                  <span className="text-xs text-slate-400">v{rollout.version}</span>
                )}
              </div>
              {/* progress bar */}
              {rollout?.progress != null && (
                <div className="space-y-1">
                  <div className="h-2 w-full overflow-hidden rounded-full bg-slate-800">
                    <div
                      className="h-full rounded-full bg-sky-500 transition-all"
                      style={{ width: `${Math.min(100, rollout.progress)}%` }}
                    />
                  </div>
                  <p className="text-xs text-slate-400">{rollout.progress}% complete</p>
                </div>
              )}
            </div>
          ) : (
            <p className="mt-2 text-sm text-slate-400">No active rollout</p>
          )}
        </div>
      </div>

      {/* rollout controls */}
      {hasActiveRollout && (
        <div className="flex flex-wrap gap-3">
          <button
            onClick={() => rolloutAction('pause')}
            disabled={!canPause}
            className="rounded-lg border border-orange-500/40 bg-orange-500/10 px-4 py-2 text-sm font-medium text-orange-300 transition hover:bg-orange-500/20 disabled:cursor-not-allowed disabled:opacity-50"
          >
            ⏸ Pause
          </button>
          <button
            onClick={() => rolloutAction('resume')}
            disabled={!canResume}
            className="rounded-lg border border-emerald-500/40 bg-emerald-500/10 px-4 py-2 text-sm font-medium text-emerald-300 transition hover:bg-emerald-500/20 disabled:cursor-not-allowed disabled:opacity-50"
          >
            ▶ Resume
          </button>
          <button
            onClick={() => rolloutAction('abort')}
            disabled={!hasActiveRollout}
            className="rounded-lg border border-rose-500/40 bg-rose-500/10 px-4 py-2 text-sm font-medium text-rose-300 transition hover:bg-rose-500/20 disabled:cursor-not-allowed disabled:opacity-50"
          >
            ✕ Abort
          </button>
        </div>
      )}

      {/* per-node versions */}
      {nodeVersions.length > 0 && (
        <div className="space-y-3">
          <h2 className="text-lg font-semibold text-slate-200">Per-Node Versions</h2>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
            {nodeVersions.map((nv) => {
              const b = badge(nv.status ?? nv.stage ?? 'unknown')
              return (
                <div
                  key={nv.name}
                  className="rounded-lg border border-slate-800 bg-slate-900/50 px-4 py-3"
                >
                  <div className="flex items-center justify-between gap-2">
                    <span className="font-medium text-slate-200">{nv.name}</span>
                    <span className={`rounded-md px-2 py-0.5 text-xs font-medium ${b.bg} ${b.text}`}>
                      {nv.status ?? nv.stage ?? '—'}
                    </span>
                  </div>
                  <p className="mt-1 text-sm text-sky-300">{nv.version}</p>
                </div>
              )
            })}
          </div>
        </div>
      )}

      {/* rollout history */}
      <div className="space-y-3">
        <h2 className="text-lg font-semibold text-slate-200">Rollout History</h2>
        {history.length === 0 ? (
          <p className="text-sm text-slate-500">No rollout history available</p>
        ) : (
          <div className="overflow-x-auto rounded-xl border border-slate-800">
            <table className="min-w-full text-sm">
              <thead>
                <tr className="border-b border-slate-800 bg-slate-900/80 text-left text-xs uppercase tracking-wider text-slate-400">
                  <th className="px-4 py-3">Version</th>
                  <th className="px-4 py-3">Date</th>
                  <th className="px-4 py-3">Status</th>
                  <th className="px-4 py-3">Duration</th>
                </tr>
              </thead>
              <tbody>
                {history.slice(0, 10).map((h, idx) => {
                  const b = badge(h.status)
                  return (
                    <tr
                      key={h.id ?? `${h.version}-${idx}`}
                      className="border-b border-slate-800/50 transition hover:bg-slate-900/40"
                    >
                      <td className="px-4 py-3 font-medium text-sky-300">{h.version}</td>
                      <td className="px-4 py-3 text-xs text-slate-400">
                        {(() => { try { return new Date(h.date).toLocaleString() } catch { return h.date } })()}
                      </td>
                      <td className="px-4 py-3">
                        <span className={`inline-block rounded-md px-2 py-0.5 text-xs font-medium ${b.bg} ${b.text}`}>
                          {h.status}
                        </span>
                      </td>
                      <td className="px-4 py-3 text-slate-400">{h.duration ?? '—'}</td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </section>
  )
}
