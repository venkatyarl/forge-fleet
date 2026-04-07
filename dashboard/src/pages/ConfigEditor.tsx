import { useCallback, useEffect, useMemo, useState } from 'react'
import { getJson, getText, parseJsonSafe } from '../lib/api'

type ReloadStatus = {
  status?: string
  message?: string
  [key: string]: unknown
}

type ErrorPayload = {
  error?: {
    message?: string
  }
}

const ACTIVATION_GUIDE = [
  'Keep secret values in host environment variables or secured fleet.toml, then restart forgefleet.',
  'After restart, use Settings to confirm enrollment token + Telegram token resolve from expected sources.',
  'Verify active database mode reports ready before enrolling additional workers.',
]

const ONBOARDING_GUIDE = [
  'Use this editor for structural config updates (nodes, loops, model routing), not secret rotation.',
  'Save config, wait for hot-reload status to return healthy, then verify impact in Fleet Overview.',
  'For new node onboarding, validate enrollment role policy and heartbeat updates after enrollment.',
]

export function ConfigEditor() {
  const [content, setContent] = useState('')
  const [savedBaseline, setSavedBaseline] = useState('')
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [status, setStatus] = useState('idle')
  const [lastSyncedAt, setLastSyncedAt] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  const hasUnsavedChanges = useMemo(() => content !== savedBaseline, [content, savedBaseline])

  const load = useCallback(async () => {
    try {
      setLoading(true)
      setError(null)

      const json = await getJson<{ content?: string }>('/api/config').catch(() => null)
      const nextContent = json?.content ?? (await getText('/api/config'))

      setContent(nextContent)
      setSavedBaseline(nextContent)
      setLastSyncedAt(new Date().toISOString())
      setStatus('loaded')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load fleet.toml')
      setStatus('error')
    } finally {
      setLoading(false)
    }
  }, [])

  const loadReloadStatus = useCallback(async () => {
    const data = await getJson<ReloadStatus>('/api/config/reload-status').catch(() => null)
    if (data?.status) {
      setStatus(data.status)
    }
  }, [])

  useEffect(() => {
    void load()
    const interval = window.setInterval(() => void loadReloadStatus(), 10000)
    return () => window.clearInterval(interval)
  }, [load, loadReloadStatus])

  const save = async () => {
    if (!hasUnsavedChanges) {
      setStatus('no changes')
      return
    }

    setSaving(true)
    setError(null)

    try {
      const res = await fetch('/api/config', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ content }),
      })

      if (!res.ok) {
        const payload = await parseJsonSafe<ErrorPayload>(res)
        throw new Error(payload?.error?.message ?? `${res.status} ${res.statusText}`)
      }

      const payload = await parseJsonSafe<ReloadStatus>(res)
      setStatus(payload?.status ?? payload?.message ?? 'saved')
      setSavedBaseline(content)
      setLastSyncedAt(new Date().toISOString())
      await loadReloadStatus()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to save configuration')
      setStatus('error')
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h2 className="text-xl font-semibold text-slate-100">Config Editor (fleet.toml)</h2>
        <div className="flex flex-wrap items-center gap-2">
          <span className="rounded-full bg-slate-800 px-2 py-1 text-xs text-slate-300">Hot-reload: {status}</span>
          <span
            className={`rounded-full px-2 py-1 text-xs ${
              hasUnsavedChanges ? 'bg-amber-500/20 text-amber-300' : 'bg-emerald-500/20 text-emerald-300'
            }`}
          >
            {hasUnsavedChanges ? 'Unsaved changes' : 'Saved'}
          </span>
          <button
            onClick={() => void load()}
            disabled={loading || saving}
            className="rounded-md border border-slate-700 bg-slate-900 px-3 py-1.5 text-sm text-slate-200 hover:border-slate-500 disabled:cursor-not-allowed disabled:opacity-60"
            type="button"
          >
            {loading ? 'Reloading…' : 'Reload'}
          </button>
          <button
            onClick={() => {
              setContent(savedBaseline)
              setStatus('reverted')
            }}
            disabled={loading || saving || !hasUnsavedChanges}
            className="rounded-md border border-slate-700 bg-slate-900 px-3 py-1.5 text-sm text-slate-200 hover:border-slate-500 disabled:cursor-not-allowed disabled:opacity-60"
            type="button"
          >
            Revert changes
          </button>
          <button
            onClick={() => void save()}
            disabled={loading || saving || !hasUnsavedChanges}
            className="rounded-md border border-sky-600/70 bg-sky-500/20 px-3 py-1.5 text-sm text-sky-200 hover:border-sky-400 disabled:cursor-not-allowed disabled:opacity-60"
            type="button"
          >
            {saving ? 'Saving...' : 'Save config'}
          </button>
        </div>
      </div>

      <Info text="Security note: keep enrollment/Telegram secrets in host env vars or secured config files; use Settings for token source and runtime health checks." />

      {lastSyncedAt ? <Info text={`Last synced: ${new Date(lastSyncedAt).toLocaleString()}`} subtle /> : null}
      {loading ? <Info text="Loading config..." /> : null}
      {error ? <Info text={`Error: ${error}`} danger /> : null}

      <textarea
        className="min-h-[420px] w-full rounded-xl border border-slate-800 bg-slate-950/80 p-4 font-mono text-sm text-slate-200 outline-none ring-sky-500 focus:ring"
        value={content}
        onChange={(event) => setContent(event.target.value)}
        spellCheck={false}
      />

      <div className="grid gap-4 xl:grid-cols-2">
        <GuidePanel title="Activation Guidance" steps={ACTIVATION_GUIDE} />
        <GuidePanel title="Onboarding Guidance" steps={ONBOARDING_GUIDE} />
      </div>
    </section>
  )
}

function GuidePanel({ title, steps }: { title: string; steps: string[] }) {
  return (
    <article className="rounded-xl border border-slate-800 bg-slate-900/50 p-4">
      <h3 className="mb-2 text-sm font-semibold uppercase tracking-wide text-slate-300">{title}</h3>
      <ol className="list-decimal space-y-2 pl-5 text-sm text-slate-300">
        {steps.map((step, idx) => (
          <li key={`${idx}-${step}`}>{step}</li>
        ))}
      </ol>
    </article>
  )
}

function Info({ text, danger = false, subtle = false }: { text: string; danger?: boolean; subtle?: boolean }) {
  return (
    <div
      className={`rounded-xl border px-4 py-3 text-sm ${
        danger
          ? 'border-rose-500/30 bg-rose-500/10 text-rose-200'
          : subtle
            ? 'border-slate-800 bg-slate-950/60 text-slate-400'
            : 'border-slate-800 bg-slate-900/50 text-slate-300'
      }`}
    >
      {text}
    </div>
  )
}
