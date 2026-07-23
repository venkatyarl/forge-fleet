import { useCallback, useEffect, useMemo, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { getJson, getText, parseJsonSafe } from '../lib/api'
import { cn } from '../lib/utils'

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
    <section className="space-y-5">
      <div className="flex flex-col gap-3 lg:flex-row lg:items-start lg:justify-between">
        <div>
          <h2 className="text-xl font-semibold text-foreground">Config Editor</h2>
          <p className="mt-1 text-sm text-muted">fleet.toml runtime configuration and hot-reload state.</p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <StatusBadge status={status}>Hot-reload: {status}</StatusBadge>
          <Badge variant={hasUnsavedChanges ? 'warn' : 'ok'}>
            {hasUnsavedChanges ? 'Unsaved changes' : 'Saved'}
          </Badge>
          {lastSyncedAt ? <Badge variant="neutral">Synced {new Date(lastSyncedAt).toLocaleString()}</Badge> : null}
          <Button onClick={() => void load()} disabled={loading || saving} type="button" variant="outline">
            {loading ? 'Reloading...' : 'Reload'}
          </Button>
          <Button
            onClick={() => {
              setContent(savedBaseline)
              setStatus('reverted')
            }}
            disabled={loading || saving || !hasUnsavedChanges}
            type="button"
            variant="secondary"
          >
            Revert changes
          </Button>
          <Button onClick={() => void save()} disabled={loading || saving || !hasUnsavedChanges} type="button">
            {saving ? 'Saving...' : 'Save config'}
          </Button>
        </div>
      </div>

      <Info text="Security note: keep enrollment/Telegram secrets in host env vars or secured config files; use Settings for token source and runtime health checks." />

      {loading ? <Info text="Loading config..." /> : null}
      {error ? <Info text={`Error: ${error}`} danger /> : null}

      <Card className="overflow-hidden bg-surface p-0">
        <CardHeader className="mb-0 border-b border-border px-4 py-3">
          <div>
            <CardTitle>fleet.toml</CardTitle>
            <CardDescription>Raw configuration loaded from /api/config.</CardDescription>
          </div>
          <Badge variant={content ? 'neutral' : 'info'}>
            {content ? `${content.split('\n').length} lines` : 'empty'}
          </Badge>
        </CardHeader>
        <textarea
          aria-label="fleet.toml contents"
          className="min-h-[520px] w-full resize-y border-0 bg-background p-4 font-mono text-sm leading-6 text-foreground outline-hidden placeholder:text-dim focus:ring-2 focus:ring-primary"
          value={content}
          onChange={(event) => setContent(event.target.value)}
          spellCheck={false}
        />
      </Card>

      <div className="grid gap-4 xl:grid-cols-2">
        <GuidePanel title="Activation Guidance" steps={ACTIVATION_GUIDE} />
        <GuidePanel title="Onboarding Guidance" steps={ONBOARDING_GUIDE} />
      </div>
    </section>
  )
}

function GuidePanel({ title, steps }: { title: string; steps: string[] }) {
  return (
    <Card className="bg-panel">
      <CardHeader>
        <div>
          <CardTitle>{title}</CardTitle>
          <CardDescription>Operational checks for config changes.</CardDescription>
        </div>
      </CardHeader>
      <ol className="list-decimal space-y-2 pl-5 text-sm text-muted">
        {steps.map((step, idx) => (
          <li key={`${idx}-${step}`}>{step}</li>
        ))}
      </ol>
    </Card>
  )
}

function Info({ text, danger = false, subtle = false }: { text: string; danger?: boolean; subtle?: boolean }) {
  return (
    <div
      className={cn(
        'rounded-xl border px-4 py-3 text-sm',
        danger
          ? 'border-status-crit bg-panel text-status-crit'
          : subtle
            ? 'border-border bg-surface text-dim'
            : 'border-border bg-panel text-muted'
      )}
    >
      {text}
    </div>
  )
}
