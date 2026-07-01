import { useEffect, useRef, useState } from 'react'
import { Badge } from '../../components/ui/badge'
import { Button } from '../../components/ui/button'
import { CardTitle } from '../../components/ui/card'
import { cn } from '../../lib/utils'

interface Msg {
  role: 'user' | 'assistant' | 'system'
  text: string
}

interface Props {
  nodeName: string
  osFamily: string
  machineKind: string
}

/**
 * Lightweight chat wedged into the onboarding page. Uses the gateway's
 * agent session API if it's present; falls back to an informational
 * message so the UI still renders even when no LLM is reachable.
 */
export function OnboardChat({ nodeName, osFamily, machineKind }: Props) {
  const [messages, setMessages] = useState<Msg[]>(() => [
    {
      role: 'system',
      text:
        `Chat context: helping onboard a ${osFamily} (${machineKind}) machine named "${nodeName}". ` +
        `Answer questions concisely with copy-paste shell commands when relevant.`,
    },
  ])
  const [input, setInput] = useState('')
  const [busy, setBusy] = useState(false)
  const scrollRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    scrollRef.current?.scrollTo(0, scrollRef.current.scrollHeight)
  }, [messages])

  const send = async () => {
    const prompt = input.trim()
    if (!prompt || busy) return
    setInput('')
    setMessages((m) => [...m, { role: 'user', text: prompt }])
    setBusy(true)
    try {
      // Use the agent session endpoint if available.
      const resp = await fetch('/api/agent/session', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          prompt,
          system_prompt:
            `You are helping onboard a new ForgeFleet node (${osFamily}, ${machineKind}, name="${nodeName}"). ` +
            `Reply in 1-3 short sentences. Prefer copy-paste shell commands when relevant.`,
          model: 'auto',
          max_turns: 1,
        }),
      })
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`)
      const data = await resp.json()
      const text =
        data.response ||
        data.content ||
        data.output ||
        data.message ||
        'Session created - streaming response not shown in this lite chat.'
      setMessages((m) => [...m, { role: 'assistant', text }])
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e)
      setMessages((m) => [
        ...m,
        {
          role: 'assistant',
          text: `Chat unavailable: ${msg}. Use ChatStudio for full chat, or SSH into the node manually.`,
        },
      ])
    } finally {
      setBusy(false)
    }
  }

  const onKey = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      send()
    }
  }

  return (
    <div className="flex h-full flex-col bg-surface">
      <div className="border-b border-border px-4 py-3">
        <CardTitle>Ask ForgeFleet</CardTitle>
        <div className="mt-1 flex flex-wrap gap-1.5">
          <Badge variant="neutral">{osFamily}</Badge>
          <Badge variant="default">{machineKind}</Badge>
        </div>
      </div>
      <div ref={scrollRef} className="flex-1 space-y-3 overflow-auto p-3 text-sm">
        {messages
          .filter((m) => m.role !== 'system')
          .map((m, i) => (
            <div
              key={i}
              className={cn(
                'rounded-lg border px-3 py-2 text-foreground',
                m.role === 'user'
                  ? 'border-primary bg-primary-subtle text-primary'
                  : 'border-border bg-panel'
              )}
            >
              <Badge variant={m.role === 'user' ? 'default' : 'neutral'}>{m.role}</Badge>
              <div className="mt-2 whitespace-pre-wrap">{m.text}</div>
            </div>
          ))}
        {busy && <div className="text-xs italic text-dim">thinking...</div>}
      </div>
      <div className="space-y-2 border-t border-border bg-panel p-3">
        <textarea
          aria-label="Onboarding chat message"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={onKey}
          rows={2}
          placeholder="Ask a question... (Enter to send, Shift+Enter for newline)"
          className="w-full resize-none rounded-lg border border-border bg-background px-3 py-2 text-sm text-foreground outline-none transition placeholder:text-dim focus:border-primary"
        />
        <div className="flex justify-end">
          <Button onClick={send} disabled={busy || input.trim() === ''} size="sm">
            Send
          </Button>
        </div>
      </div>
    </div>
  )
}
