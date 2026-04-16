import { useEffect, useRef, useState } from 'react'

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
        'Session created — streaming response not shown in this lite chat.'
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
    <div className="h-full flex flex-col bg-slate-950 border-l border-slate-800">
      <div className="px-3 py-2 border-b border-slate-800 text-xs uppercase tracking-wider text-slate-400">
        Ask ForgeFleet
      </div>
      <div ref={scrollRef} className="flex-1 overflow-auto p-3 space-y-3 text-sm">
        {messages
          .filter((m) => m.role !== 'system')
          .map((m, i) => (
            <div
              key={i}
              className={
                m.role === 'user'
                  ? 'bg-indigo-600/20 rounded px-3 py-2 text-slate-100'
                  : 'bg-slate-800 rounded px-3 py-2 text-slate-100'
              }
            >
              <div className="text-[10px] uppercase tracking-wider text-slate-500 mb-1">
                {m.role}
              </div>
              <div className="whitespace-pre-wrap">{m.text}</div>
            </div>
          ))}
        {busy && <div className="text-xs text-slate-500 italic">thinking…</div>}
      </div>
      <div className="border-t border-slate-800 p-2">
        <textarea
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={onKey}
          rows={2}
          placeholder="Ask a question… (Enter to send, Shift+Enter for newline)"
          className="w-full bg-slate-900 border border-slate-700 rounded px-2 py-1.5 text-sm text-slate-100 placeholder:text-slate-500 resize-none"
        />
      </div>
    </div>
  )
}
