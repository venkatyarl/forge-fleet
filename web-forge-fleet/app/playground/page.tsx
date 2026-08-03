'use client'

import { useCallback, useEffect, useRef, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { FlaskConical, Send, Square, Trash2, TriangleAlert } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { StandaloneTopBar } from '@/components/standalone-top-bar'
import { getJson } from '@/lib/api'
import { apiUrl } from '@/lib/gateway'
import { cn } from '@/lib/utils'

type ChatMessage = { role: 'user' | 'assistant'; content: string }

type OpenAiModelsResponse = { data?: { id?: string }[] }

function useModels() {
  return useQuery({
    queryKey: ['playground', 'models'],
    staleTime: 60_000,
    queryFn: async () => {
      const payload = await getJson<OpenAiModelsResponse>('/v1/models')
      if (!Array.isArray(payload?.data)) return []
      return payload.data
        .map((m) => m?.id)
        .filter((id): id is string => typeof id === 'string' && id.length > 0)
    },
  })
}

export default function PlaygroundPage() {
  const models = useModels()
  const [model, setModel] = useState('')
  const [messages, setMessages] = useState<ChatMessage[]>([])
  const [input, setInput] = useState('')
  const [streaming, setStreaming] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [temperature, setTemperature] = useState(0.7)
  const [maxTokens, setMaxTokens] = useState(1024)

  const abortRef = useRef<AbortController | null>(null)
  const bottomRef = useRef<HTMLDivElement | null>(null)
  const inputRef = useRef<HTMLTextAreaElement | null>(null)

  // Default the picker to the first model once the list arrives.
  useEffect(() => {
    if (!model && models.data && models.data.length > 0) {
      setModel(models.data[0])
    }
  }, [models.data, model])

  // Scroll to the newest content as tokens stream in.
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth', block: 'end' })
  }, [messages])

  // Abort an in-flight request if the page unmounts.
  useEffect(() => () => abortRef.current?.abort(), [])

  const stop = useCallback(() => {
    abortRef.current?.abort()
  }, [])

  const send = useCallback(async () => {
    const prompt = input.trim()
    if (!prompt || streaming) return
    if (!model) {
      setError('No model selected — check that the gateway is serving /v1/models.')
      return
    }

    setError(null)
    setInput('')

    const history = [...messages, { role: 'user', content: prompt } as ChatMessage]
    // Append an empty assistant bubble that the stream fills in.
    setMessages([...history, { role: 'assistant', content: '' }])
    setStreaming(true)

    const controller = new AbortController()
    abortRef.current = controller

    try {
      const res = await fetch(apiUrl('/v1/chat/completions'), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          model,
          messages: history,
          temperature,
          max_tokens: maxTokens,
          stream: true,
        }),
        signal: controller.signal,
      })

      if (!res.ok) {
        throw new Error(`${res.status} ${res.statusText}`)
      }
      if (!res.body) {
        throw new Error('Response has no body to stream')
      }

      const reader = res.body.getReader()
      const decoder = new TextDecoder()
      let buffer = ''
      let done = false

      const appendDelta = (delta: string) => {
        setMessages((prev) => {
          const next = [...prev]
          const last = next[next.length - 1]
          if (last?.role === 'assistant') {
            next[next.length - 1] = { ...last, content: last.content + delta }
          }
          return next
        })
      }

      const handleLine = (line: string): boolean => {
        const trimmed = line.trim()
        if (!trimmed.startsWith('data:')) return false
        const data = trimmed.slice(5).trim()
        if (data === '[DONE]') return true
        try {
          const json = JSON.parse(data) as {
            choices?: { delta?: { content?: string }; message?: { content?: string } }[]
          }
          const delta =
            json.choices?.[0]?.delta?.content ?? json.choices?.[0]?.message?.content ?? ''
          if (delta) appendDelta(delta)
        } catch {
          // Partial JSON chunk — ignore; SSE framing will resync on the next line.
        }
        return false
      }

      while (!done) {
        const { value, done: readerDone } = await reader.read()
        if (readerDone) break
        buffer += decoder.decode(value, { stream: true })
        const lines = buffer.split('\n')
        buffer = lines.pop() ?? ''
        for (const line of lines) {
          if (handleLine(line)) {
            done = true
            break
          }
        }
      }
      if (!done && buffer) handleLine(buffer)
    } catch (err) {
      if (err instanceof DOMException && err.name === 'AbortError') {
        // User stopped the stream — keep whatever tokens arrived.
      } else {
        setError(err instanceof Error ? err.message : 'Request failed')
        // Drop the empty assistant bubble if nothing ever streamed.
        setMessages((prev) => {
          const last = prev[prev.length - 1]
          return last?.role === 'assistant' && last.content === '' ? prev.slice(0, -1) : prev
        })
      }
    } finally {
      abortRef.current = null
      setStreaming(false)
      inputRef.current?.focus()
    }
  }, [input, streaming, model, messages, temperature, maxTokens])

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      void send()
    }
  }

  return (
    <div className="flex min-h-screen flex-col bg-background">
      <StandaloneTopBar title="Playground" subtitle="Chat with any model on the fleet">
        <Badge variant={models.isError ? 'crit' : models.isSuccess ? 'ok' : 'info'}>
          {models.isError
            ? 'models unavailable'
            : models.isSuccess
              ? `${models.data?.length ?? 0} models`
              : 'loading'}
        </Badge>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          onClick={() => setMessages([])}
          disabled={streaming || messages.length === 0}
        >
          <Trash2 className="h-3.5 w-3.5" />
          Clear
        </Button>
      </StandaloneTopBar>

      {/* Controls */}
      <div className="border-b border-border bg-surface/50">
        <div className="mx-auto flex max-w-4xl flex-wrap items-center gap-x-5 gap-y-2 px-4 py-3 sm:px-6">
          <label className="flex items-center gap-2 text-2xs font-medium tracking-wider text-dim uppercase">
            Model
            <select
              value={model}
              onChange={(e) => setModel(e.target.value)}
              disabled={streaming}
              className="h-8 max-w-56 rounded-lg border border-border bg-panel px-2 font-mono text-xs normal-case text-foreground disabled:opacity-50"
            >
              {models.data?.length ? null : <option value="">no models</option>}
              {models.data?.map((id) => (
                <option key={id} value={id}>
                  {id}
                </option>
              ))}
            </select>
          </label>
          <label className="flex items-center gap-2 text-2xs font-medium tracking-wider text-dim uppercase">
            Temp
            <input
              type="range"
              min={0}
              max={2}
              step={0.1}
              value={temperature}
              onChange={(e) => setTemperature(Number(e.target.value))}
              disabled={streaming}
              className="h-1 w-24 accent-primary"
            />
            <span className="w-6 font-mono text-xs text-muted normal-case">
              {temperature.toFixed(1)}
            </span>
          </label>
          <label className="flex items-center gap-2 text-2xs font-medium tracking-wider text-dim uppercase">
            Max tokens
            <input
              type="number"
              min={1}
              max={32768}
              value={maxTokens}
              onChange={(e) => setMaxTokens(Math.max(1, Number(e.target.value) || 1))}
              disabled={streaming}
              className="h-8 w-20 rounded-lg border border-border bg-panel px-2 font-mono text-xs normal-case text-foreground disabled:opacity-50"
            />
          </label>
        </div>
      </div>

      {/* Messages */}
      <main className="mx-auto flex w-full max-w-4xl flex-1 flex-col px-4 py-6 sm:px-6">
        {messages.length === 0 ? (
          <div className="flex flex-1 flex-col items-center justify-center gap-3 text-center">
            <div className="flex h-12 w-12 items-center justify-center rounded-2xl bg-primary-subtle">
              <FlaskConical className="h-6 w-6 text-primary-muted" />
            </div>
            <p className="text-sm font-medium text-foreground">Start a conversation</p>
            <p className="max-w-sm text-xs text-dim">
              Pick a model above and send a message. Responses stream token-by-token from the
              fleet&apos;s OpenAI-compatible endpoint.
            </p>
          </div>
        ) : (
          <div className="flex flex-1 flex-col gap-4">
            {messages.map((message, idx) => (
              <div
                key={idx}
                className={cn(
                  'max-w-[85%] rounded-2xl px-4 py-3 text-sm leading-relaxed break-words whitespace-pre-wrap',
                  message.role === 'user'
                    ? 'self-end rounded-br-md bg-primary text-white'
                    : 'self-start rounded-bl-md border border-border bg-panel font-mono text-[13px] text-foreground'
                )}
              >
                {message.content}
                {message.role === 'assistant' &&
                streaming &&
                idx === messages.length - 1 ? (
                  <span className="ml-0.5 inline-block h-3.5 w-1.5 animate-pulse bg-primary-muted align-text-bottom" />
                ) : null}
              </div>
            ))}
            <div ref={bottomRef} />
          </div>
        )}

        {error ? (
          <div className="mt-4 flex items-start gap-2 rounded-lg border border-rose-500/30 bg-rose-500/10 px-3 py-2 text-xs text-status-crit">
            <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span className="font-mono">{error}</span>
          </div>
        ) : null}

        {/* Composer */}
        <div className="sticky bottom-0 mt-6 bg-background pt-2 pb-4">
          <div className="flex items-end gap-2 rounded-2xl border border-border bg-panel p-2 focus-within:border-primary/50">
            <textarea
              ref={inputRef}
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={onKeyDown}
              placeholder={streaming ? 'Waiting for response…' : 'Message the fleet…  (Enter to send, Shift+Enter for newline)'}
              rows={Math.min(6, Math.max(1, input.split('\n').length))}
              disabled={streaming}
              className="max-h-40 flex-1 resize-none bg-transparent px-2 py-1.5 text-sm text-foreground placeholder:text-dim focus:outline-none disabled:opacity-60"
            />
            {streaming ? (
              <Button type="button" variant="secondary" size="icon" onClick={stop} aria-label="Stop">
                <Square className="h-3.5 w-3.5" />
              </Button>
            ) : (
              <Button
                type="button"
                size="icon"
                onClick={() => void send()}
                disabled={!input.trim() || !model}
                aria-label="Send"
              >
                <Send className="h-3.5 w-3.5" />
              </Button>
            )}
          </div>
        </div>
      </main>
    </div>
  )
}
