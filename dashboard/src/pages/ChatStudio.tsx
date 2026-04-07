import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { FormEvent } from 'react'

type BackendTarget = {
  id: string
  label: string
  baseUrl: string
}

type ModelOption = {
  id: string
  tier?: number
  ownedBy?: string
}

type ChatRole = 'system' | 'user' | 'assistant' | 'error' | 'tool_start' | 'tool_end' | 'status'

type ChatHistoryEntry = {
  id: string
  role: ChatRole
  content: string
  at: string
  model?: string
  backend?: string
  toolName?: string
  toolInput?: string
  toolOutput?: string
  isError?: boolean
  durationMs?: number
}

type ModelListPayload = {
  data?: Array<{
    id?: string
    name?: string
    tier?: number
    owned_by?: string
  }>
  models?: Array<{
    id?: string
    name?: string
    tier?: number
    owned_by?: string
  }>
}

type ChatCompletionRequestMessage = {
  role: 'system' | 'user' | 'assistant'
  content: string
}

type AgentEvent = {
  event: string
  session_id: string
  tool_name?: string
  tool_id?: string
  input_json?: string
  result?: string
  is_error?: boolean
  duration_ms?: number
  text?: string
  final_text?: string
  turn?: number
  finish_reason?: string
  message?: string
}

function asRecord(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null
  return value as Record<string, unknown>
}

function toApiUrl(baseUrl: string, path: string): string {
  if (!baseUrl) return path
  const normalized = baseUrl.replace(/\/+$/, '')
  const suffix = path.startsWith('/') ? path : `/${path}`
  return `${normalized}${suffix}`
}

function getBackendTargets(): BackendTarget[] {
  const targets: BackendTarget[] = [{ id: 'gateway', label: 'Gateway Router (same origin)', baseUrl: '' }]

  const current = new URL(window.location.href)
  const currentPort = Number.parseInt(current.port || '0', 10)

  if (Number.isFinite(currentPort) && currentPort > 1) {
    const directPort = currentPort - 1
    if (directPort > 0 && directPort !== currentPort) {
      targets.push({
        id: 'direct-api',
        label: `Direct API Service (${directPort})`,
        baseUrl: `${current.protocol}//${current.hostname}:${directPort}`,
      })
    }
  }

  targets.push({ id: 'custom', label: 'Custom Backend URL', baseUrl: '' })
  return targets
}

function normalizeModelList(payload: ModelListPayload): ModelOption[] {
  const source = Array.isArray(payload.data) ? payload.data : Array.isArray(payload.models) ? payload.models : []

  const seen = new Set<string>()
  const models: ModelOption[] = []

  for (const item of source) {
    const id = item.id ?? item.name
    if (!id || seen.has(id)) continue
    seen.add(id)
    models.push({ id, tier: item.tier, ownedBy: item.owned_by })
  }

  return models
}

function stringifyContent(content: unknown): string {
  if (typeof content === 'string') return content

  if (Array.isArray(content)) {
    return content
      .map((item) => {
        if (typeof item === 'string') return item
        const part = asRecord(item)
        if (!part) return JSON.stringify(item)
        if (typeof part.text === 'string') return part.text
        if (typeof part.content === 'string') return part.content
        return JSON.stringify(part)
      })
      .join('\n')
      .trim()
  }

  const record = asRecord(content)
  if (record) {
    if (typeof record.text === 'string') return record.text
    if (typeof record.content === 'string') return record.content
  }

  if (content === null || content === undefined) return ''
  return JSON.stringify(content, null, 2)
}

function extractAssistantText(payload: unknown): string | null {
  const root = asRecord(payload)
  if (!root) return null

  const choices = Array.isArray(root.choices) ? root.choices : []
  const firstChoice = asRecord(choices[0])
  if (!firstChoice) return null

  const message = asRecord(firstChoice.message)
  if (message && 'content' in message) {
    const content = stringifyContent(message.content)
    return content || null
  }

  if ('text' in firstChoice) {
    const text = stringifyContent(firstChoice.text)
    return text || null
  }

  return null
}

function buildRequestMessages(history: ChatHistoryEntry[], systemPrompt: string): ChatCompletionRequestMessage[] {
  const messages: ChatCompletionRequestMessage[] = []

  const trimmedSystemPrompt = systemPrompt.trim()
  if (trimmedSystemPrompt) {
    messages.push({ role: 'system', content: trimmedSystemPrompt })
  }

  for (const entry of history) {
    if (entry.role === 'user' || entry.role === 'assistant') {
      messages.push({ role: entry.role, content: entry.content })
    }
  }

  return messages
}

async function parseErrorMessage(res: Response): Promise<string> {
  const body = await res.text()

  if (body) {
    try {
      const parsed = JSON.parse(body) as { error?: { message?: string } }
      const detailed = parsed?.error?.message
      if (typeof detailed === 'string' && detailed.trim()) {
        return `${res.status} ${res.statusText}: ${detailed}`
      }
    } catch {
      // fall through to raw body
    }

    return `${res.status} ${res.statusText}: ${body.slice(0, 280)}`
  }

  return `${res.status} ${res.statusText}`
}

function makeId(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`
}

// ─── Fleet LLM Endpoints ────────────────────────────────────────────────────

const FLEET_LLMS = [
  { label: 'Marcus — Qwen2.5-Coder-32B', url: 'http://192.168.5.102:51000' },
  { label: 'Sophie — Qwen2.5-Coder-32B', url: 'http://192.168.5.103:51000' },
  { label: 'Priya — Qwen2.5-Coder-32B', url: 'http://192.168.5.104:51000' },
  { label: 'Taylor — Gemma 4 31B', url: 'http://192.168.5.100:51000' },
  { label: 'Taylor — Qwen3 Coder Next', url: 'http://192.168.5.100:51001' },
  { label: 'James — Qwen2.5-72B', url: 'http://192.168.5.108:51000' },
  { label: 'James — Qwen3.5-9B', url: 'http://192.168.5.108:51001' },
]

// ─── Component ──────────────────────────────────────────────────────────────

export function ChatStudio() {
  const backendTargets = useMemo(() => getBackendTargets(), [])

  const [backendId, setBackendId] = useState<string>('gateway')
  const [customBackendUrl, setCustomBackendUrl] = useState('')
  const [models, setModels] = useState<ModelOption[]>([])
  const [selectedModel, setSelectedModel] = useState('')
  const [systemPrompt, setSystemPrompt] = useState('')
  const [draft, setDraft] = useState('')
  const [history, setHistory] = useState<ChatHistoryEntry[]>([])
  const [loadingModels, setLoadingModels] = useState(true)
  const [sending, setSending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Agent mode state
  const [agentMode, setAgentMode] = useState(false)
  const [agentSessionId, setAgentSessionId] = useState<string | null>(null)
  const [agentRunning, setAgentRunning] = useState(false)
  const [selectedFleetLlm, setSelectedFleetLlm] = useState(FLEET_LLMS[0].url)
  const wsRef = useRef<WebSocket | null>(null)
  const chatEndRef = useRef<HTMLDivElement | null>(null)

  // Auto-scroll to bottom
  useEffect(() => {
    chatEndRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [history])

  const activeBackend = useMemo<BackendTarget>(() => {
    if (backendId === 'custom') {
      return {
        id: 'custom',
        label: 'Custom Backend URL',
        baseUrl: customBackendUrl.trim().replace(/\/+$/, ''),
      }
    }

    return backendTargets.find((target) => target.id === backendId) ?? backendTargets[0]
  }, [backendId, backendTargets, customBackendUrl])

  const loadModels = useCallback(async () => {
    if (backendId === 'custom' && !activeBackend.baseUrl) {
      setModels([])
      setSelectedModel('')
      setError('Enter a custom backend URL to load models.')
      setLoadingModels(false)
      return
    }

    setLoadingModels(true)
    setError(null)

    try {
      const primary = await fetch(toApiUrl(activeBackend.baseUrl, '/v1/models'))

      let payload: ModelListPayload
      if (primary.ok) {
        payload = (await primary.json()) as ModelListPayload
      } else if (primary.status === 404) {
        const fallback = await fetch(toApiUrl(activeBackend.baseUrl, '/api/models'))
        if (!fallback.ok) {
          throw new Error(await parseErrorMessage(fallback))
        }
        payload = (await fallback.json()) as ModelListPayload
      } else {
        throw new Error(await parseErrorMessage(primary))
      }

      const nextModels = normalizeModelList(payload)
      setModels(nextModels)
      setSelectedModel((prev) => {
        if (prev && nextModels.some((model) => model.id === prev)) return prev
        return nextModels[0]?.id ?? ''
      })
    } catch (err) {
      setModels([])
      setSelectedModel('')
      setError(err instanceof Error ? err.message : 'Failed to load models from backend')
    } finally {
      setLoadingModels(false)
    }
  }, [activeBackend.baseUrl, backendId])

  useEffect(() => {
    void loadModels()
  }, [loadModels])

  // WebSocket connection for agent events
  useEffect(() => {
    if (!agentMode) {
      wsRef.current?.close()
      wsRef.current = null
      return
    }

    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const ws = new WebSocket(`${proto}//${window.location.host}/ws`)

    ws.onopen = () => {
      ws.send(JSON.stringify({ type: 'subscribe', events: ['agent_event'] }))
    }

    ws.onmessage = (ev) => {
      try {
        const envelope = JSON.parse(ev.data as string) as { type: string; data: AgentEvent }
        if (envelope.type !== 'agent_event') return

        const data = envelope.data
        if (!data?.event) return

        switch (data.event) {
          case 'tool_start':
            setHistory((prev) => [
              ...prev,
              {
                id: makeId('tool-start'),
                role: 'tool_start',
                content: `Calling ${data.tool_name}...`,
                at: new Date().toISOString(),
                toolName: data.tool_name,
                toolInput: data.input_json,
              },
            ])
            break

          case 'tool_end':
            setHistory((prev) => [
              ...prev,
              {
                id: makeId('tool-end'),
                role: 'tool_end',
                content: data.result ?? '',
                at: new Date().toISOString(),
                toolName: data.tool_name,
                isError: data.is_error,
                durationMs: data.duration_ms,
              },
            ])
            break

          case 'assistant_text':
            if (data.text) {
              setHistory((prev) => [
                ...prev,
                {
                  id: makeId('assistant'),
                  role: 'assistant',
                  content: data.text!,
                  at: new Date().toISOString(),
                },
              ])
            }
            break

          case 'status':
            setHistory((prev) => [
              ...prev,
              {
                id: makeId('status'),
                role: 'status',
                content: data.message ?? '',
                at: new Date().toISOString(),
              },
            ])
            break

          case 'error':
            setHistory((prev) => [
              ...prev,
              {
                id: makeId('error'),
                role: 'error',
                content: data.message ?? 'Unknown error',
                at: new Date().toISOString(),
              },
            ])
            setAgentRunning(false)
            break

          case 'done':
            setAgentRunning(false)
            break
        }
      } catch {
        // ignore parse errors
      }
    }

    ws.onclose = () => {
      wsRef.current = null
    }

    wsRef.current = ws

    return () => {
      ws.close()
    }
  }, [agentMode])

  const sendMessage = useCallback(
    async (event: FormEvent<HTMLFormElement>) => {
      event.preventDefault()

      const text = draft.trim()
      if (!text || sending || agentRunning) return

      setError(null)
      setDraft('')

      const userEntry: ChatHistoryEntry = {
        id: makeId('user'),
        role: 'user',
        content: text,
        at: new Date().toISOString(),
        model: selectedModel,
        backend: activeBackend.label,
      }

      setHistory((prev) => [...prev, userEntry])

      if (agentMode) {
        // Agent mode: POST to /api/agent/session
        setSending(true)
        setAgentRunning(true)

        try {
          const resp = await fetch('/api/agent/session', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              prompt: text,
              llm_base_url: selectedFleetLlm,
              system_prompt: systemPrompt.trim() || undefined,
            }),
          })

          if (!resp.ok) {
            throw new Error(await parseErrorMessage(resp))
          }

          const result = (await resp.json()) as { session_id: string }
          setAgentSessionId(result.session_id)
        } catch (err) {
          const message = err instanceof Error ? err.message : 'Failed to create agent session'
          setError(message)
          setAgentRunning(false)
          setHistory((prev) => [
            ...prev,
            { id: makeId('error'), role: 'error', content: message, at: new Date().toISOString() },
          ])
        } finally {
          setSending(false)
        }
      } else {
        // Standard chat mode
        if (!selectedModel) {
          setError('Select a model before sending a message.')
          return
        }

        if (backendId === 'custom' && !activeBackend.baseUrl) {
          setError('Enter a custom backend URL before sending.')
          return
        }

        setSending(true)

        try {
          const nextHistory = [...history, userEntry]
          const response = await fetch(toApiUrl(activeBackend.baseUrl, '/v1/chat/completions'), {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              model: selectedModel,
              stream: false,
              messages: buildRequestMessages(nextHistory, systemPrompt),
            }),
          })

          if (!response.ok) {
            throw new Error(await parseErrorMessage(response))
          }

          const payload = (await response.json()) as unknown
          const assistantText = extractAssistantText(payload) ?? stringifyContent(payload)

          setHistory((prev) => [
            ...prev,
            {
              id: makeId('assistant'),
              role: 'assistant',
              content: assistantText,
              at: new Date().toISOString(),
              model: selectedModel,
              backend: activeBackend.label,
            },
          ])
        } catch (err) {
          const message = err instanceof Error ? err.message : 'Chat request failed'
          setError(message)
          setHistory((prev) => [
            ...prev,
            { id: makeId('error'), role: 'error', content: message, at: new Date().toISOString(), model: selectedModel, backend: activeBackend.label },
          ])
        } finally {
          setSending(false)
        }
      }
    },
    [activeBackend.baseUrl, activeBackend.label, agentMode, agentRunning, backendId, draft, history, selectedFleetLlm, selectedModel, sending, systemPrompt],
  )

  const cancelAgent = useCallback(async () => {
    if (!agentSessionId) return
    try {
      await fetch(`/api/agent/session/${agentSessionId}/cancel`, { method: 'POST' })
    } catch {
      // ignore
    }
    setAgentRunning(false)
  }, [agentSessionId])

  return (
    <section className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-xl font-semibold text-slate-100">Chat Studio</h2>
          <p className="text-sm text-slate-400">
            {agentMode ? 'Agent mode — LLM-driven tool execution on your fleet' : 'OpenAI-compatible chat wired to live ForgeFleet LLM routes'}
          </p>
        </div>
        <div className="flex items-center gap-3">
          <label className="flex items-center gap-2 text-sm text-slate-300 cursor-pointer">
            <div
              className={`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${agentMode ? 'bg-violet-500' : 'bg-slate-700'}`}
              onClick={() => setAgentMode(!agentMode)}
            >
              <span
                className={`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${agentMode ? 'translate-x-6' : 'translate-x-1'}`}
              />
            </div>
            Agent Mode
          </label>
          {!agentMode && (
            <button
              onClick={() => void loadModels()}
              className="rounded-md border border-slate-700 bg-slate-900 px-3 py-1.5 text-sm text-slate-200 hover:border-slate-500"
              type="button"
            >
              Reload Models
            </button>
          )}
        </div>
      </div>

      <div className="grid gap-3 rounded-xl border border-slate-800 bg-slate-900/70 p-4 lg:grid-cols-3">
        {agentMode ? (
          <>
            <label className="grid gap-1 text-sm text-slate-300 lg:col-span-3">
              Fleet LLM Endpoint
              <select
                className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-100"
                value={selectedFleetLlm}
                onChange={(e) => setSelectedFleetLlm(e.target.value)}
              >
                {FLEET_LLMS.map((llm) => (
                  <option key={llm.url} value={llm.url}>
                    {llm.label}
                  </option>
                ))}
              </select>
            </label>
          </>
        ) : (
          <>
            <label className="grid gap-1 text-sm text-slate-300">
              Backend Target
              <select
                className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-100"
                value={backendId}
                onChange={(event) => setBackendId(event.target.value)}
              >
                {backendTargets.map((target) => (
                  <option key={target.id} value={target.id}>
                    {target.label}
                  </option>
                ))}
              </select>
            </label>

            <label className="grid gap-1 text-sm text-slate-300 lg:col-span-2">
              Model
              <select
                className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-100"
                value={selectedModel}
                onChange={(event) => setSelectedModel(event.target.value)}
                disabled={loadingModels || models.length === 0}
              >
                {models.map((model) => (
                  <option key={model.id} value={model.id}>
                    {model.id}
                    {typeof model.tier === 'number' ? ` (tier ${model.tier})` : ''}
                    {model.ownedBy ? ` · ${model.ownedBy}` : ''}
                  </option>
                ))}
                {models.length === 0 ? <option value="">No models available</option> : null}
              </select>
            </label>

            {backendId === 'custom' ? (
              <label className="grid gap-1 text-sm text-slate-300 lg:col-span-3">
                Custom Backend Base URL
                <input
                  className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-100"
                  type="url"
                  value={customBackendUrl}
                  placeholder="https://host:port"
                  onChange={(event) => setCustomBackendUrl(event.target.value)}
                />
              </label>
            ) : null}
          </>
        )}

        <label className="grid gap-1 text-sm text-slate-300 lg:col-span-3">
          System Prompt (optional)
          <textarea
            className="min-h-[76px] rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-100"
            value={systemPrompt}
            onChange={(event) => setSystemPrompt(event.target.value)}
            placeholder={agentMode ? 'Custom agent instructions...' : 'You are ForgeFleet\'s assistant...'}
          />
        </label>
      </div>

      {error ? (
        <div className="rounded-xl border border-rose-500/30 bg-rose-500/10 px-4 py-3 text-sm text-rose-200">{error}</div>
      ) : null}

      <div className="rounded-xl border border-slate-800 bg-slate-900/70">
        <div className="border-b border-slate-800 px-4 py-3 text-sm text-slate-400 flex items-center justify-between">
          <span>
            {agentMode
              ? `Agent Mode · ${FLEET_LLMS.find((l) => l.url === selectedFleetLlm)?.label ?? selectedFleetLlm}${agentRunning ? ' · Running...' : ''}`
              : loadingModels
                ? 'Loading models...'
                : `Backend: ${activeBackend.label} · Model: ${selectedModel || 'none selected'}`}
          </span>
          {agentRunning && (
            <button
              onClick={() => void cancelAgent()}
              className="rounded-md border border-rose-500/40 bg-rose-500/20 px-3 py-1 text-xs text-rose-200 hover:bg-rose-500/30"
              type="button"
            >
              Cancel
            </button>
          )}
        </div>

        <div className="max-h-[520px] space-y-3 overflow-y-auto p-4">
          {history.length === 0 ? (
            <p className="text-sm text-slate-500">
              {agentMode
                ? 'Agent mode active. Send a prompt and the agent will use tools (Bash, Read, Edit, etc.) to complete the task.'
                : 'No messages yet. Send a prompt to start this session.'}
            </p>
          ) : (
            history.map((entry) => {
              if (entry.role === 'tool_start') {
                return (
                  <article key={entry.id} className="rounded-lg border border-amber-500/30 bg-amber-500/5 p-3 text-sm">
                    <header className="flex items-center gap-2 text-xs text-amber-400">
                      <span className="font-mono font-semibold">{entry.toolName}</span>
                      <span className="text-slate-500">{new Date(entry.at).toLocaleTimeString()}</span>
                    </header>
                    {entry.toolInput && (
                      <pre className="mt-1 max-h-32 overflow-auto whitespace-pre-wrap break-words text-xs text-slate-400 font-mono">
                        {entry.toolInput}
                      </pre>
                    )}
                  </article>
                )
              }

              if (entry.role === 'tool_end') {
                return (
                  <article
                    key={entry.id}
                    className={`rounded-lg border p-3 text-sm ${
                      entry.isError
                        ? 'border-rose-500/30 bg-rose-500/5'
                        : 'border-emerald-500/30 bg-emerald-500/5'
                    }`}
                  >
                    <header className="flex items-center gap-2 text-xs">
                      <span className={`font-mono font-semibold ${entry.isError ? 'text-rose-400' : 'text-emerald-400'}`}>
                        {entry.toolName} {entry.isError ? '(error)' : ''}
                      </span>
                      {entry.durationMs !== undefined && (
                        <span className="text-slate-500">{entry.durationMs}ms</span>
                      )}
                    </header>
                    <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap break-words text-xs text-slate-300 font-mono">
                      {entry.content}
                    </pre>
                  </article>
                )
              }

              if (entry.role === 'status') {
                return (
                  <div key={entry.id} className="text-xs text-slate-500 text-center py-1">
                    {entry.content}
                  </div>
                )
              }

              return (
                <article
                  key={entry.id}
                  className={`rounded-lg border p-3 text-sm ${
                    entry.role === 'user'
                      ? 'border-sky-500/30 bg-sky-500/10 text-sky-100'
                      : entry.role === 'assistant'
                        ? 'border-emerald-500/30 bg-emerald-500/10 text-emerald-100'
                        : entry.role === 'error'
                          ? 'border-rose-500/30 bg-rose-500/10 text-rose-100'
                          : 'border-slate-700 bg-slate-950 text-slate-100'
                  }`}
                >
                  <header className="mb-2 flex flex-wrap items-center justify-between gap-2 text-xs uppercase tracking-wide text-slate-400">
                    <span>{entry.role}</span>
                    <span>{new Date(entry.at).toLocaleTimeString()}</span>
                  </header>
                  <pre className="whitespace-pre-wrap break-words font-sans leading-6">{entry.content}</pre>
                </article>
              )
            })
          )}
          <div ref={chatEndRef} />
        </div>
      </div>

      <form onSubmit={sendMessage} className="space-y-3 rounded-xl border border-slate-800 bg-slate-900/70 p-4">
        <label className="grid gap-1 text-sm text-slate-300">
          Message
          <textarea
            className="min-h-[100px] rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-100"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            placeholder={agentMode ? 'Tell the agent what to do...' : 'Ask ForgeFleet to route this prompt across your model fleet...'}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                e.preventDefault()
                e.currentTarget.form?.requestSubmit()
              }
            }}
          />
        </label>

        <div className="flex flex-wrap items-center justify-between gap-2">
          <p className="text-xs text-slate-400">
            {agentMode ? 'Cmd+Enter to send. Agent events stream via WebSocket.' : 'History is kept in this page state for the current browser tab.'}
          </p>
          <button
            type="submit"
            disabled={sending || agentRunning || !draft.trim() || (!agentMode && !selectedModel) || (!agentMode && backendId === 'custom' && !activeBackend.baseUrl)}
            className={`rounded-md border px-4 py-2 text-sm font-medium transition disabled:cursor-not-allowed disabled:opacity-50 ${
              agentMode
                ? 'border-violet-500/40 bg-violet-500/20 text-violet-100 hover:bg-violet-500/30'
                : 'border-sky-500/40 bg-sky-500/20 text-sky-100 hover:bg-sky-500/30'
            }`}
          >
            {sending ? 'Sending...' : agentRunning ? 'Agent Running...' : agentMode ? 'Run Agent' : 'Send'}
          </button>
        </div>
      </form>
    </section>
  )
}
