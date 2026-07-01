import { useCallback, useEffect, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { deleteJson, getJson, patchJson, postJson } from '../lib/api'
import { cn } from '../lib/utils'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type UnifiedModel = {
  name: string
  params?: string
  context?: string
  bestFor?: string
  sources: ModelSource[]
  /** Fleet-specific */
  fleetMember?: string
  fleetIp?: string
  fleetPort?: number
  online?: boolean
}

type ModelSource = {
  name: 'Fleet'
}

type LlmServer = {
  computer: string
  endpoint: string
  runtime: string
  model: string
  queue_depth: number
  active_requests?: number | null
  tokens_per_sec: number
  gpu_pct?: number | null
  load_score?: number | null
  healthy: boolean
  status: string
  enabled?: boolean
}

type ServerForm = {
  computer: string
  endpoint: string
  runtime: string
  model: string
  enabled: boolean
}

const EMPTY_FORM: ServerForm = {
  computer: '',
  endpoint: '',
  runtime: '',
  model: '',
  enabled: true,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function parseParamsFromModelName(name: string): string | undefined {
  const m = name.match(/(?:^|[-_/])(\d+\.?\d*)B/i)
  return m ? m[1] + 'B' : undefined
}

function parseContextFromName(_name: string): string | undefined {
  return undefined
}

function endpointToIpPort(endpoint: string): { ip: string; port: number } | null {
  try {
    const url = new URL(endpoint)
    return { ip: url.hostname, port: parseInt(url.port, 10) || 80 }
  } catch {
    return null
  }
}

function serverIdentity(server: LlmServer) {
  return {
    computer: server.computer,
    endpoint: server.endpoint,
    model: server.model,
  }
}

function serverKey(server: LlmServer, index: number) {
  return `${server.computer}-${server.endpoint}-${server.model}-${index}`
}

function serverEnabled(server: LlmServer) {
  if (typeof server.enabled === 'boolean') return server.enabled
  return server.status.toLowerCase() !== 'disabled'
}

function modelRowsFromServers(servers: LlmServer[]): UnifiedModel[] {
  return servers.map((s) => {
    const ep = endpointToIpPort(s.endpoint)
    return {
      name: s.model,
      params: parseParamsFromModelName(s.model),
      context: parseContextFromName(s.model),
      bestFor: s.runtime,
      fleetMember: s.computer,
      fleetIp: ep?.ip,
      fleetPort: ep?.port,
      online: s.healthy,
      sources: [{ name: 'Fleet' }],
    }
  })
}

function toForm(server: LlmServer): ServerForm {
  return {
    computer: server.computer,
    endpoint: server.endpoint,
    runtime: server.runtime,
    model: server.model,
    enabled: serverEnabled(server),
  }
}

// ---------------------------------------------------------------------------
// Main component
// ---------------------------------------------------------------------------

export function ModelHub() {
  const [query, setQuery] = useState('')
  const [models, setModels] = useState<UnifiedModel[]>([])
  const [servers, setServers] = useState<LlmServer[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [editing, setEditing] = useState<LlmServer | null>(null)
  const [form, setForm] = useState<ServerForm>(EMPTY_FORM)

  const loadServers = useCallback(async () => {
    setLoading(true)
    try {
      setError(null)
      const res = await getJson<{ servers: LlmServer[] }>('/api/llm/servers')
      const nextServers = res.servers || []
      setServers(nextServers)
      setModels(modelRowsFromServers(nextServers))
    } catch (err) {
      setServers([])
      setModels([])
      setError(err instanceof Error ? err.message : 'Failed to load LLM servers')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void loadServers()
  }, [loadServers])

  const filtered = models.filter((m) => {
    if (!query) return true
    const lower = query.toLowerCase()
    return (
      m.name.toLowerCase().includes(lower) ||
      (m.bestFor ?? '').toLowerCase().includes(lower) ||
      (m.fleetMember ?? '').toLowerCase().includes(lower) ||
      (m.params ?? '').toLowerCase().includes(lower) ||
      m.sources.some((s) => s.name.toLowerCase().includes(lower))
    )
  })

  const healthyServers = servers.filter((server) => server.healthy).length
  const totalQueue = servers.reduce((sum, server) => sum + (server.queue_depth || 0), 0)
  const totalTokens = servers.reduce((sum, server) => sum + (server.tokens_per_sec || 0), 0)

  const updateForm = (field: keyof ServerForm, value: string | boolean) => {
    setForm((prev) => ({ ...prev, [field]: value }))
  }

  const resetForm = () => {
    setEditing(null)
    setForm(EMPTY_FORM)
    setActionError(null)
  }

  const submitServer = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    setSaving(true)
    setActionError(null)

    const payload = {
      computer: form.computer.trim(),
      endpoint: form.endpoint.trim(),
      runtime: form.runtime.trim(),
      model: form.model.trim(),
      enabled: form.enabled,
    }

    try {
      if (editing) {
        await patchJson('/api/llm/servers', { ...payload, previous: serverIdentity(editing) })
      } else {
        await postJson('/api/llm/servers', payload)
      }
      resetForm()
      await loadServers()
    } catch (err) {
      setActionError(err instanceof Error ? err.message : 'Failed to save LLM server')
    } finally {
      setSaving(false)
    }
  }

  const deleteServer = async (server: LlmServer) => {
    setSaving(true)
    setActionError(null)
    try {
      await deleteJson('/api/llm/servers', serverIdentity(server))
      await loadServers()
      if (editing && serverIdentity(editing).endpoint === server.endpoint) {
        resetForm()
      }
    } catch (err) {
      setActionError(err instanceof Error ? err.message : 'Failed to delete LLM server')
    } finally {
      setSaving(false)
    }
  }

  const setServerEnabled = async (server: LlmServer, enabled: boolean) => {
    setSaving(true)
    setActionError(null)
    try {
      await patchJson('/api/llm/servers', { ...serverIdentity(server), enabled })
      await loadServers()
    } catch (err) {
      setActionError(err instanceof Error ? err.message : 'Failed to update LLM server')
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <h2 className="text-xl font-semibold text-foreground">Model Hub</h2>
          <p className="mt-1 text-sm text-muted">
            Manage fleet LLM servers and browse models available to ForgeFleet.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {loading ? <Badge variant="info">loading</Badge> : <Badge variant="ok">live</Badge>}
          <Button onClick={() => void loadServers()} type="button" variant="outline" disabled={loading || saving}>
            Refresh
          </Button>
        </div>
      </div>

      <div className="grid gap-3 md:grid-cols-4">
        <Stat label="LLM Servers" value={String(servers.length)} />
        <Stat label="Healthy" value={`${healthyServers}/${servers.length}`} tone={healthyServers === servers.length ? 'ok' : 'warn'} />
        <Stat label="Queue Depth" value={String(totalQueue)} />
        <Stat label="Tokens / Sec" value={totalTokens.toFixed(1)} tone="info" />
      </div>

      {error ? <Notice tone="crit" text={`Server inventory failed: ${error}`} /> : null}
      {actionError ? <Notice tone="crit" text={`Server action failed: ${actionError}`} /> : null}

      <div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_22rem]">
        <Card className="overflow-hidden bg-surface p-0">
          <CardHeader className="mb-0 border-b border-border px-4 py-3">
            <div>
              <CardTitle>LLM Servers</CardTitle>
              <CardDescription>Live server status from /api/llm/servers.</CardDescription>
            </div>
            <Badge variant="neutral">{servers.length} rows</Badge>
          </CardHeader>
          <div className="overflow-x-auto">
            <table className="min-w-full text-left text-sm">
              <thead className="border-b border-border bg-elevated text-xs uppercase text-dim">
                <tr>
                  <th className="px-4 py-2 font-medium">Computer</th>
                  <th className="px-4 py-2 font-medium">Model</th>
                  <th className="px-4 py-2 font-medium">Runtime</th>
                  <th className="px-4 py-2 font-medium">Endpoint</th>
                  <th className="px-4 py-2 font-medium">Queue</th>
                  <th className="px-4 py-2 font-medium">Tok/s</th>
                  <th className="px-4 py-2 font-medium">Status</th>
                  <th className="px-4 py-2 text-right font-medium">Actions</th>
                </tr>
              </thead>
              <tbody>
                {servers.length === 0 ? (
                  <tr>
                    <td className="px-4 py-8 text-center text-sm text-dim" colSpan={8}>
                      {loading ? 'Loading LLM servers...' : 'No LLM servers reported yet.'}
                    </td>
                  </tr>
                ) : (
                  servers.map((server, index) => {
                    const enabled = serverEnabled(server)
                    return (
                      <tr
                        key={serverKey(server, index)}
                        className="border-t border-border text-muted transition hover:bg-panel hover:text-foreground"
                      >
                        <td className="px-4 py-3 text-foreground">{server.computer || '-'}</td>
                        <td className="px-4 py-3 font-mono text-xs text-status-info">{server.model || '-'}</td>
                        <td className="px-4 py-3">{server.runtime || '-'}</td>
                        <td className="max-w-72 truncate px-4 py-3 font-mono text-xs text-dim">
                          {server.endpoint || '-'}
                        </td>
                        <td className="px-4 py-3 text-foreground">{server.queue_depth ?? '-'}</td>
                        <td className="px-4 py-3 text-foreground">
                          {server.tokens_per_sec == null ? '-' : server.tokens_per_sec.toFixed(1)}
                        </td>
                        <td className="px-4 py-3">
                          <StatusBadge status={enabled ? server.status || 'unknown' : 'disabled'}>
                            {enabled ? server.status || 'unknown' : 'disabled'}
                          </StatusBadge>
                        </td>
                        <td className="px-4 py-3">
                          <div className="flex justify-end gap-2">
                            <Button
                              onClick={() => {
                                setEditing(server)
                                setForm(toForm(server))
                                setActionError(null)
                              }}
                              type="button"
                              variant="secondary"
                              size="sm"
                              disabled={saving}
                            >
                              Edit
                            </Button>
                            <Button
                              onClick={() => void setServerEnabled(server, !enabled)}
                              type="button"
                              variant="outline"
                              size="sm"
                              disabled={saving}
                            >
                              {enabled ? 'Disable' : 'Enable'}
                            </Button>
                            <Button
                              onClick={() => void deleteServer(server)}
                              type="button"
                              variant="ghost"
                              size="sm"
                              className="text-status-crit hover:text-status-crit"
                              disabled={saving}
                            >
                              Delete
                            </Button>
                          </div>
                        </td>
                      </tr>
                    )
                  })
                )}
              </tbody>
            </table>
          </div>
        </Card>

        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>{editing ? 'Edit Server' : 'Add Server'}</CardTitle>
              <CardDescription>{editing ? editing.endpoint : 'Register a managed LLM endpoint.'}</CardDescription>
            </div>
          </CardHeader>
          <form className="space-y-3" onSubmit={(event) => void submitServer(event)}>
            <Field
              label="Computer"
              value={form.computer}
              onChange={(value) => updateForm('computer', value)}
              placeholder="taylor"
              required
            />
            <Field
              label="Endpoint"
              value={form.endpoint}
              onChange={(value) => updateForm('endpoint', value)}
              placeholder="http://192.168.5.100:51000"
              required
            />
            <Field
              label="Runtime"
              value={form.runtime}
              onChange={(value) => updateForm('runtime', value)}
              placeholder="llama.cpp"
              required
            />
            <Field
              label="Model"
              value={form.model}
              onChange={(value) => updateForm('model', value)}
              placeholder="Qwen2.5-Coder-32B"
              required
            />
            <label className="flex items-center gap-2 rounded-lg border border-border bg-surface px-3 py-2 text-sm text-muted">
              <input
                checked={form.enabled}
                className="h-4 w-4 accent-primary"
                onChange={(event) => updateForm('enabled', event.target.checked)}
                type="checkbox"
              />
              Enabled
            </label>
            <div className="flex gap-2">
              <Button className="flex-1" disabled={saving} type="submit">
                {saving ? 'Saving...' : editing ? 'Save Changes' : 'Add Server'}
              </Button>
              {editing ? (
                <Button onClick={resetForm} type="button" variant="outline" disabled={saving}>
                  Cancel
                </Button>
              ) : null}
            </div>
          </form>
        </Card>
      </div>

      <Card className="overflow-hidden bg-surface p-0">
        <CardHeader className="mb-0 border-b border-border px-4 py-3">
          <div>
            <CardTitle>Available Models</CardTitle>
            <CardDescription>Fleet models merged with well-known external references.</CardDescription>
          </div>
          <Badge variant="neutral">
            {loading ? 'loading' : `${filtered.length} model${filtered.length !== 1 ? 's' : ''}`}
          </Badge>
        </CardHeader>

        <div className="border-b border-border bg-panel px-4 py-3">
          <div className="flex flex-col gap-3 sm:flex-row sm:items-center">
            <input
              type="text"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search models by name, params, use case..."
              aria-label="Search models"
              className="min-h-9 w-full rounded-lg border border-border bg-background px-3 py-2 text-sm text-foreground placeholder:text-dim focus:border-primary focus:outline-none"
            />
          </div>
        </div>

        <div className="overflow-x-auto">
          <table className="min-w-full text-left text-sm">
            <thead className="border-b border-border bg-elevated text-xs uppercase text-dim">
              <tr>
                <th className="px-4 py-2 font-medium">Model</th>
                <th className="px-4 py-2 font-medium">Params</th>
                <th className="px-4 py-2 font-medium">Context</th>
                <th className="px-4 py-2 font-medium">Best For</th>
                <th className="px-4 py-2 font-medium">Fleet Member</th>
                <th className="px-4 py-2 font-medium">Sources</th>
                <th className="px-4 py-2 font-medium">Status</th>
              </tr>
            </thead>
            <tbody>
              {filtered.map((m, i) => (
                <tr
                  key={`${m.name}-${m.fleetMember ?? i}`}
                  className="border-t border-border text-muted transition hover:bg-panel hover:text-foreground"
                >
                  <td className="px-4 py-3 font-medium text-foreground">{m.name}</td>
                  <td className="px-4 py-3">{m.params ?? '-'}</td>
                  <td className="px-4 py-3">{m.context ?? '-'}</td>
                  <td className="px-4 py-3">{m.bestFor ?? '-'}</td>
                  <td className="px-4 py-3">
                    {m.fleetMember ? <span className="text-foreground">{m.fleetMember}</span> : <span className="text-dim">-</span>}
                  </td>
                  <td className="px-4 py-3">
                    <div className="flex flex-wrap gap-1">
                      {m.sources.map((s) => (
                        <SourceBadge key={s.name} source={s} />
                      ))}
                    </div>
                  </td>
                  <td className="px-4 py-3">
                    {m.fleetMember ? (
                      <StatusBadge status={m.online ? 'online' : 'offline'}>{m.online ? 'Online' : 'Offline'}</StatusBadge>
                    ) : (
                      <Badge variant="neutral">Available</Badge>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>

          {filtered.length === 0 && !loading ? (
            <div className="px-4 py-8 text-center text-sm text-dim">
              No models match &ldquo;{query}&rdquo;
            </div>
          ) : null}
        </div>
      </Card>
    </section>
  )
}

function Stat({ label, value, tone = 'neutral' }: { label: string; value: string; tone?: 'neutral' | 'ok' | 'warn' | 'info' }) {
  return (
    <Card className="bg-panel">
      <CardHeader className="mb-2">
        <CardDescription className="uppercase tracking-wide">{label}</CardDescription>
      </CardHeader>
      <p
        className={cn(
          'text-2xl font-semibold text-foreground',
          tone === 'ok' && 'text-status-ok',
          tone === 'warn' && 'text-status-warn',
          tone === 'info' && 'text-status-info'
        )}
      >
        {value}
      </p>
    </Card>
  )
}

function Notice({ text, tone }: { text: string; tone: 'crit' | 'info' }) {
  return (
    <div
      className={cn(
        'rounded-xl border px-4 py-3 text-sm',
        tone === 'crit'
          ? 'border-status-crit bg-panel text-status-crit'
          : 'border-status-info bg-panel text-status-info'
      )}
    >
      {text}
    </div>
  )
}

function Field({
  label,
  value,
  onChange,
  placeholder,
  required = false,
}: {
  label: string
  value: string
  onChange: (value: string) => void
  placeholder?: string
  required?: boolean
}) {
  return (
    <label className="block space-y-1 text-sm">
      <span className="text-dim">{label}</span>
      <input
        className="min-h-9 w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-foreground placeholder:text-dim focus:border-primary focus:outline-none"
        onChange={(event) => onChange(event.target.value)}
        placeholder={placeholder}
        required={required}
        type="text"
        value={value}
      />
    </label>
  )
}

// ---------------------------------------------------------------------------
// Source badge with link
// ---------------------------------------------------------------------------

function SourceBadge({ source }: { source: ModelSource }) {
  return (
    <Badge variant="default" className="bg-primary-subtle text-primary">
      {source.name}
    </Badge>
  )
}
