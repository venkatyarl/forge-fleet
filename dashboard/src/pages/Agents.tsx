import { useCallback, useEffect, useMemo, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { getJson, postJson } from '../lib/api'
import { cn } from '../lib/utils'

type LiveSession = {
  session_id: string
  status: string
  model: string
  llm_base_url: string
  created_at: string
}

type V54Session = {
  session_id?: string
  id?: string
  status?: string
  model?: string
  prompt?: string
  verdict?: string
  members?: string[]
  created_at?: string
}

type SessionsPayload = {
  sessions: LiveSession[]
  v54_sessions: V54Session[]
}

type DetailPayload = {
  session_id: string
  status: string
  model: string
  llm_base_url: string
  created_at: string
  brain?: unknown[]
}

function statusTone(status?: string) {
  const s = (status ?? '').toLowerCase()
  if (s === 'running') return 'running'
  if (s === 'done' || s === 'completed' || s === 'success') return 'success'
  if (s === 'error' || s === 'failed') return 'error'
  if (s === 'cancelled') return 'warning'
  return 'neutral'
}

export function Agents() {
  const [data, setData] = useState<SessionsPayload | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [detail, setDetail] = useState<DetailPayload | null>(null)
  const [detailLoading, setDetailLoading] = useState(false)
  const [creating, setCreating] = useState(false)

  const [form, setForm] = useState({
    prompt: '',
    model: '',
    llm_base_url: '',
    working_dir: '',
    system_prompt: '',
  })

  const load = useCallback(async () => {
    try {
      setError(null)
      const payload = await getJson<SessionsPayload>('/api/agent/sessions')
      setData(payload)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load agent sessions')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
    const id = window.setInterval(() => void load(), 5000)
    return () => window.clearInterval(id)
  }, [load])

  const selectedV54 = useMemo(() => {
    if (!selectedId || !data) return null
    return data.v54_sessions.find((s) => (s.session_id ?? s.id) === selectedId) ?? null
  }, [selectedId, data])

  useEffect(() => {
    if (!selectedId || selectedV54) {
      setDetail(null)
      return
    }
    let cancelled = false
    async function fetchDetail() {
      setDetailLoading(true)
      try {
        const payload = await getJson<DetailPayload>(`/api/agent/session/${selectedId}/status`)
        if (!cancelled) setDetail(payload)
      } catch {
        if (!cancelled) setDetail(null)
      } finally {
        if (!cancelled) setDetailLoading(false)
      }
    }
    void fetchDetail()
    const id = window.setInterval(fetchDetail, 3000)
    return () => {
      cancelled = true
      window.clearInterval(id)
    }
  }, [selectedId, selectedV54])

  const createSession = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!form.prompt.trim()) return
    setCreating(true)
    try {
      const payload = await postJson<LiveSession>('/api/agent/session', {
        prompt: form.prompt,
        ...(form.model ? { model: form.model } : {}),
        ...(form.llm_base_url ? { llm_base_url: form.llm_base_url } : {}),
        ...(form.working_dir ? { working_dir: form.working_dir } : {}),
        ...(form.system_prompt ? { system_prompt: form.system_prompt } : {}),
      })
      if (!payload) throw new Error('No response from agent session creation')
      setSelectedId(payload.session_id)
      setForm((f) => ({ ...f, prompt: '' }))
      await load()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create session')
    } finally {
      setCreating(false)
    }
  }

  const cancelSession = async (id: string) => {
    try {
      await postJson(`/api/agent/session/${id}/cancel`, {})
      await load()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to cancel session')
    }
  }

  const liveCount = data?.sessions.length ?? 0
  const v54Count = data?.v54_sessions.length ?? 0

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Agents & Swarm</h1>
            {loading ? <Badge variant="info">loading</Badge> : <Badge variant="ok">live</Badge>}
          </div>
          <p className="mt-1 text-sm text-muted">
            Live agent sessions and V54 multi-LLM outcome-driven runs.
          </p>
        </div>
        <Button onClick={() => void load()} disabled={loading} type="button" variant="outline">
          Refresh
        </Button>
      </div>

      <div className="grid gap-3 sm:grid-cols-3">
        <SummaryCard label="Live Sessions" value={liveCount.toLocaleString()} detail="in-memory agent sessions" />
        <SummaryCard label="V54 Runs" value={v54Count.toLocaleString()} detail="outcome-driven multi-LLM sessions" />
        <SummaryCard
          label="Active"
          value={data?.sessions.filter((s) => s.status.toLowerCase() === 'running').length.toLocaleString() ?? '0'}
          detail="currently running"
        />
      </div>

      {error ? (
        <Card className="border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">{error}</Card>
      ) : null}

      <div className="grid gap-4 xl:grid-cols-12">
        <div className="space-y-4 xl:col-span-4">
          <Card className="bg-panel">
            <CardHeader>
              <div>
                <CardTitle>New Agent Session</CardTitle>
                <CardDescription>Dispatch a single-turn or multi-turn agent.</CardDescription>
              </div>
            </CardHeader>
            <form onSubmit={createSession} className="space-y-3">
              <textarea
                aria-label="Agent prompt"
                value={form.prompt}
                onChange={(e) => setForm((f) => ({ ...f, prompt: e.target.value }))}
                placeholder="Prompt..."
                rows={3}
                required
                className={fieldClass}
              />
              <div className="grid gap-3 sm:grid-cols-2">
                <input
                  aria-label="Agent model"
                  value={form.model}
                  onChange={(e) => setForm((f) => ({ ...f, model: e.target.value }))}
                  placeholder="Model (optional)"
                  className={fieldClass}
                />
                <input
                  aria-label="LLM base URL"
                  value={form.llm_base_url}
                  onChange={(e) => setForm((f) => ({ ...f, llm_base_url: e.target.value }))}
                  placeholder="LLM base URL (optional)"
                  className={fieldClass}
                />
                <input
                  aria-label="Working directory"
                  value={form.working_dir}
                  onChange={(e) => setForm((f) => ({ ...f, working_dir: e.target.value }))}
                  placeholder="Working dir (optional)"
                  className={fieldClass}
                />
                <input
                  aria-label="System prompt"
                  value={form.system_prompt}
                  onChange={(e) => setForm((f) => ({ ...f, system_prompt: e.target.value }))}
                  placeholder="System prompt (optional)"
                  className={fieldClass}
                />
              </div>
              <Button type="submit" disabled={creating || !form.prompt.trim()} className="w-full">
                {creating ? 'Starting...' : 'Start session'}
              </Button>
            </form>
          </Card>

          <Card className="bg-panel">
            <CardHeader>
              <div>
                <CardTitle>Live Sessions</CardTitle>
                <CardDescription>{liveCount} session{liveCount === 1 ? '' : 's'}</CardDescription>
              </div>
            </CardHeader>
            {loading && liveCount === 0 ? (
              <p className="text-sm text-dim">Loading sessions...</p>
            ) : liveCount === 0 ? (
              <p className="text-sm text-dim">No live agent sessions.</p>
            ) : (
              <div className="space-y-2">
                {data?.sessions.map((s) => (
                  <button
                    key={s.session_id}
                    type="button"
                    onClick={() => setSelectedId(s.session_id)}
                    className={cn(
                      'w-full rounded-lg border p-3 text-left transition',
                      selectedId === s.session_id
                        ? 'border-primary bg-primary-subtle'
                        : 'border-border bg-surface hover:border-border-subtle hover:bg-elevated',
                    )}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <p className="min-w-0 flex-1 truncate font-mono text-xs text-status-info">{s.session_id}</p>
                      <StatusBadge status={statusTone(s.status)}>{s.status}</StatusBadge>
                    </div>
                    <p className="mt-1 truncate text-sm text-foreground">{s.model}</p>
                    <p className="text-xs text-dim">{new Date(s.created_at).toLocaleString()}</p>
                    {s.status.toLowerCase() === 'running' && (
                      <Button
                        type="button"
                        variant="outline"
                        size="sm"
                        className="mt-2 h-6 text-xs"
                        onClick={(e) => {
                          e.stopPropagation()
                          void cancelSession(s.session_id)
                        }}
                      >
                        Cancel
                      </Button>
                    )}
                  </button>
                ))}
              </div>
            )}
          </Card>
        </div>

        <div className="space-y-4 xl:col-span-4">
          <Card className="bg-panel">
            <CardHeader>
              <div>
                <CardTitle>V54 Swarm Runs</CardTitle>
                <CardDescription>{v54Count} outcome-driven run{v54Count === 1 ? '' : 's'}</CardDescription>
              </div>
            </CardHeader>
            {loading && v54Count === 0 ? (
              <p className="text-sm text-dim">Loading V54 sessions...</p>
            ) : v54Count === 0 ? (
              <p className="text-sm text-dim">No V54 sessions recorded.</p>
            ) : (
              <div className="space-y-2">
                {data?.v54_sessions.map((s) => {
                  const id = s.session_id ?? s.id ?? ''
                  return (
                    <button
                      key={id}
                      type="button"
                      onClick={() => setSelectedId(id)}
                      className={cn(
                        'w-full rounded-lg border p-3 text-left transition',
                        selectedId === id
                          ? 'border-primary bg-primary-subtle'
                          : 'border-border bg-surface hover:border-border-subtle hover:bg-elevated',
                      )}
                    >
                      <div className="flex items-start justify-between gap-2">
                        <p className="min-w-0 flex-1 truncate font-mono text-xs text-status-info">{id}</p>
                        <StatusBadge status={statusTone(s.status)}>{s.status ?? 'unknown'}</StatusBadge>
                      </div>
                      {s.members && s.members.length > 0 && (
                        <div className="mt-2 flex flex-wrap gap-1">
                          {s.members.map((m) => (
                            <Badge key={m} variant="neutral">
                              {m}
                            </Badge>
                          ))}
                        </div>
                      )}
                      {s.prompt ? <p className="mt-1 line-clamp-2 text-xs text-muted">{s.prompt}</p> : null}
                    </button>
                  )
                })}
              </div>
            )}
          </Card>
        </div>

        <div className="xl:col-span-4">
          <Card className="bg-panel">
            <CardHeader>
              <div>
                <CardTitle>Session Detail</CardTitle>
                <CardDescription>
                  {selectedId ? (selectedV54 ? 'V54 outcome-driven session' : 'Live session status') : 'Select a session'}
                </CardDescription>
              </div>
            </CardHeader>
            {!selectedId ? (
              <p className="text-sm text-dim">Select a session to inspect details.</p>
            ) : detailLoading && !detail && !selectedV54 ? (
              <p className="text-sm text-dim">Loading detail...</p>
            ) : selectedV54 ? (
              <div className="space-y-3 text-sm">
                <DetailRow label="ID" value={selectedV54.session_id ?? selectedV54.id ?? '-'} />
                <DetailRow label="Status" value={selectedV54.status ?? '-'} />
                <DetailRow label="Model" value={selectedV54.model ?? '-'} />
                {selectedV54.members && selectedV54.members.length > 0 && (
                  <div className="flex flex-wrap gap-1">
                    {selectedV54.members.map((m) => (
                      <Badge key={m} variant="neutral">
                        {m}
                      </Badge>
                    ))}
                  </div>
                )}
                {selectedV54.prompt ? (
                  <div className="rounded-lg border border-border bg-surface p-2">
                    <p className="text-xs uppercase text-dim">Prompt</p>
                    <p className="mt-1 whitespace-pre-wrap text-muted">{selectedV54.prompt}</p>
                  </div>
                ) : null}
                {selectedV54.verdict ? (
                  <div className="rounded-lg border border-border bg-surface p-2">
                    <p className="text-xs uppercase text-dim">Verdict</p>
                    <p className="mt-1 whitespace-pre-wrap text-foreground">{selectedV54.verdict}</p>
                  </div>
                ) : null}
              </div>
            ) : detail ? (
              <div className="space-y-3 text-sm">
                <DetailRow label="ID" value={detail.session_id} />
                <DetailRow label="Status" value={detail.status} />
                <DetailRow label="Model" value={detail.model} />
                <DetailRow label="LLM" value={detail.llm_base_url} />
                <DetailRow label="Created" value={new Date(detail.created_at).toLocaleString()} />
                {Array.isArray(detail.brain) && detail.brain.length > 0 && (
                  <div className="rounded-lg border border-border bg-surface p-2">
                    <p className="text-xs uppercase text-dim">Brain entries</p>
                    <p className="mt-1 text-muted">{detail.brain.length} entries</p>
                  </div>
                )}
              </div>
            ) : (
              <p className="text-sm text-dim">No detail available.</p>
            )}
          </Card>
        </div>
      </div>
    </section>
  )
}

function SummaryCard({ label, value, detail }: { label: string; value: string; detail: string }) {
  return (
    <Card className="bg-panel px-4 py-3">
      <CardDescription className="uppercase tracking-wide">{label}</CardDescription>
      <div className="mt-1 text-2xl font-semibold text-foreground">{value}</div>
      <p className="mt-1 text-xs text-dim">{detail}</p>
    </Card>
  )
}

function DetailRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="grid grid-cols-[4rem_minmax(0,1fr)] gap-3 text-xs">
      <dt className="font-mono uppercase text-dim">{label}</dt>
      <dd className="min-w-0 break-all font-mono text-muted">{value}</dd>
    </div>
  )
}

const fieldClass =
  'min-h-9 w-full rounded-lg border border-border bg-elevated px-3 py-2 text-sm text-foreground outline-hidden transition placeholder:text-dim focus:border-primary disabled:cursor-not-allowed disabled:opacity-60'
