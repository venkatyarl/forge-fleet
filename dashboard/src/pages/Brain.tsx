import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useNavigate, useParams } from 'react-router-dom'
import { getJson, postJson } from '../lib/api'

/* ------------------------------------------------------------------ */
/*  Types                                                              */
/* ------------------------------------------------------------------ */

interface BrainThread {
  id: string
  slug: string
  title: string
  project: string
  status: string
  last_message_at: string
}

interface BrainMessage {
  id: string
  role: 'user' | 'assistant' | 'system'
  content: string
  channel?: string
  created_at: string
}

interface StackItem {
  title: string
  context?: string
  progress?: number
  pushed_at: string
}

interface BacklogItem {
  title: string
  priority: 'urgent' | 'high' | 'medium' | 'low'
  created_at: string
}

/* ------------------------------------------------------------------ */
/*  Helpers                                                            */
/* ------------------------------------------------------------------ */

function relTime(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime()
  const mins = Math.floor(diff / 60_000)
  if (mins < 1) return 'now'
  if (mins < 60) return `${mins}m`
  const hrs = Math.floor(mins / 60)
  if (hrs < 24) return `${hrs}h`
  const days = Math.floor(hrs / 24)
  return `${days}d`
}

const priorityEmoji: Record<string, string> = {
  urgent: '\u{1F534}',
  high: '\u{1F7E0}',
  medium: '\u{1F7E1}',
  low: '\u{1F7E2}',
}

/* ------------------------------------------------------------------ */
/*  Thread Sidebar                                                     */
/* ------------------------------------------------------------------ */

function ThreadSidebar({
  threads,
  activeSlug,
  searchQuery,
  onSearchChange,
}: {
  threads: BrainThread[]
  activeSlug: string | undefined
  searchQuery: string
  onSearchChange: (q: string) => void
}) {
  const navigate = useNavigate()
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({})

  const grouped = useMemo(() => {
    const map = new Map<string, BrainThread[]>()
    const filtered = threads.filter(
      (t) =>
        !searchQuery ||
        t.title.toLowerCase().includes(searchQuery.toLowerCase()) ||
        t.project.toLowerCase().includes(searchQuery.toLowerCase()),
    )
    for (const t of filtered) {
      const list = map.get(t.project) || []
      list.push(t)
      map.set(t.project, list)
    }
    return map
  }, [threads, searchQuery])

  return (
    <div className="flex h-full w-60 flex-shrink-0 flex-col border-r border-slate-800 bg-slate-950/60">
      <div className="p-2">
        <input
          type="text"
          value={searchQuery}
          onChange={(e) => onSearchChange(e.target.value)}
          placeholder="Search threads..."
          className="w-full rounded border border-slate-700 bg-slate-900 px-2 py-1.5 text-xs text-slate-200 placeholder-slate-500 outline-none focus:border-violet-500"
        />
      </div>
      <div className="flex-1 overflow-y-auto px-1">
        {Array.from(grouped.entries()).map(([project, items]) => (
          <div key={project} className="mb-1">
            <button
              onClick={() => setCollapsed((p) => ({ ...p, [project]: !p[project] }))}
              className="flex w-full items-center gap-1.5 rounded px-2 py-1 text-[11px] font-semibold uppercase tracking-wider text-slate-500 hover:text-slate-300"
            >
              <span className={`text-[10px] transition-transform ${collapsed[project] ? '-rotate-90' : ''}`}>
                ▾
              </span>
              {project}
            </button>
            {!collapsed[project] &&
              items.map((t) => (
                <button
                  key={t.id}
                  onClick={() => navigate(`/brain/${t.slug}`)}
                  className={`flex w-full items-center justify-between rounded px-3 py-1.5 text-left text-sm transition ${
                    activeSlug === t.slug
                      ? 'bg-violet-500/15 text-violet-300'
                      : 'text-slate-400 hover:bg-slate-800/70 hover:text-slate-200'
                  }`}
                >
                  <span className="truncate">{t.title}</span>
                  <span className="ml-2 flex-shrink-0 text-[10px] text-slate-600">
                    {relTime(t.last_message_at)}
                  </span>
                </button>
              ))}
          </div>
        ))}
        {grouped.size === 0 && (
          <div className="px-3 py-4 text-xs text-slate-600">No threads found</div>
        )}
      </div>
    </div>
  )
}

/* ------------------------------------------------------------------ */
/*  Chat Panel                                                         */
/* ------------------------------------------------------------------ */

function ChatPanel({
  threadSlug,
  messages,
  loading,
  error,
}: {
  threadSlug: string
  messages: BrainMessage[]
  loading: boolean
  error: string | null
}) {
  const [input, setInput] = useState('')
  const [sending, setSending] = useState(false)
  const bottomRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  const send = async () => {
    const text = input.trim()
    if (!text || sending) return
    setSending(true)
    setInput('')
    try {
      await postJson(`/api/brain/threads/${threadSlug}/message`, { content: text })
    } catch {
      // message will appear on next poll if the API comes online
    } finally {
      setSending(false)
    }
  }

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      send()
    }
  }

  return (
    <div className="flex flex-1 flex-col">
      <div className="border-b border-slate-800 px-4 py-2 text-sm font-medium text-slate-300">
        Thread: {threadSlug}
      </div>

      <div className="flex-1 overflow-y-auto px-4 py-3 space-y-3">
        {loading && <div className="text-sm text-slate-500">Loading messages...</div>}
        {error && <div className="text-sm text-rose-400">Error: {error}</div>}
        {!loading && !error && messages.length === 0 && (
          <div className="text-sm text-slate-600">No messages yet. Start the conversation below.</div>
        )}
        {messages.map((m) => (
          <div
            key={m.id}
            className={`max-w-[80%] rounded-lg px-3 py-2 text-sm ${
              m.role === 'user'
                ? 'ml-auto bg-violet-500/20 text-violet-100'
                : m.role === 'assistant'
                  ? 'bg-slate-800/60 text-slate-200'
                  : 'bg-slate-900/40 text-slate-500 italic text-xs'
            }`}
          >
            <div className="mb-0.5 text-[10px] font-semibold uppercase tracking-wider text-slate-500">
              {m.role}
              {m.channel && <span className="ml-1 text-slate-600">via {m.channel}</span>}
            </div>
            <div className="whitespace-pre-wrap">{m.content}</div>
          </div>
        ))}
        <div ref={bottomRef} />
      </div>

      <div className="border-t border-slate-800 p-3">
        <div className="flex gap-2">
          <textarea
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={onKeyDown}
            placeholder="Send a message..."
            rows={1}
            className="flex-1 resize-none rounded border border-slate-700 bg-slate-900 px-3 py-2 text-sm text-slate-200 placeholder-slate-500 outline-none focus:border-violet-500"
          />
          <button
            onClick={send}
            disabled={sending || !input.trim()}
            className="rounded border border-violet-500 px-4 py-2 text-sm font-medium text-violet-300 transition hover:bg-violet-500/20 disabled:opacity-40"
          >
            {sending ? '...' : 'Send'}
          </button>
        </div>
      </div>
    </div>
  )
}

/* ------------------------------------------------------------------ */
/*  Right Panel: Stack + Backlog                                       */
/* ------------------------------------------------------------------ */

function RightPanel({
  threadSlug,
  project,
}: {
  threadSlug: string | undefined
  project: string | undefined
}) {
  const [stackItems, setStackItems] = useState<StackItem[]>([])
  const [backlogItems, setBacklogItems] = useState<BacklogItem[]>([])

  useEffect(() => {
    if (!threadSlug) {
      setStackItems([])
      return
    }
    getJson<{ items: StackItem[] }>(`/api/brain/stack/${threadSlug}`)
      .then((d) => setStackItems(d.items || []))
      .catch(() => setStackItems([]))
  }, [threadSlug])

  useEffect(() => {
    if (!project) {
      setBacklogItems([])
      return
    }
    getJson<{ items: BacklogItem[] }>(`/api/brain/backlog/${project}`)
      .then((d) => setBacklogItems(d.items || []))
      .catch(() => setBacklogItems([]))
  }, [project])

  return (
    <div className="flex h-full w-[280px] flex-shrink-0 flex-col border-l border-slate-800 bg-slate-950/60 overflow-y-auto">
      {/* Stack */}
      <div className="border-b border-slate-800 p-3">
        <div className="mb-2 text-[11px] font-semibold uppercase tracking-wider text-slate-500">
          Stack {threadSlug && <span className="text-slate-600">({threadSlug})</span>}
        </div>
        {stackItems.length === 0 ? (
          <div className="text-xs text-slate-600">No stack items</div>
        ) : (
          <ul className="space-y-1">
            {stackItems.map((it, i) => (
              <li key={i} className="flex items-start gap-1.5 text-sm text-slate-300">
                <span className="mt-0.5 text-slate-600">▸</span>
                <span className="flex-1 truncate">{it.title}</span>
                {it.progress != null && (
                  <span className="flex-shrink-0 text-[10px] text-slate-500">
                    {Math.round(it.progress * 100)}%
                  </span>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>

      {/* Backlog */}
      <div className="p-3">
        <div className="mb-2 text-[11px] font-semibold uppercase tracking-wider text-slate-500">
          Backlog {project && <span className="text-slate-600">({project})</span>}
        </div>
        {backlogItems.length === 0 ? (
          <div className="text-xs text-slate-600">No backlog items</div>
        ) : (
          <ul className="space-y-1">
            {backlogItems.map((it, i) => (
              <li key={i} className="flex items-start gap-1.5 text-sm text-slate-300">
                <span className="mt-0.5">{priorityEmoji[it.priority] || ''}</span>
                <span className="flex-1 truncate">{it.title}</span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  )
}

/* ------------------------------------------------------------------ */
/*  Fuzzy Thread Picker (Cmd+K)                                        */
/* ------------------------------------------------------------------ */

function FuzzyPicker({
  threads,
  onSelect,
  onClose,
}: {
  threads: BrainThread[]
  onSelect: (slug: string) => void
  onClose: () => void
}) {
  const [q, setQ] = useState('')
  const inputRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    inputRef.current?.focus()
  }, [])

  const filtered = useMemo(() => {
    if (!q) return threads.slice(0, 20)
    const lq = q.toLowerCase()
    return threads.filter(
      (t) =>
        t.title.toLowerCase().includes(lq) ||
        t.slug.toLowerCase().includes(lq) ||
        t.project.toLowerCase().includes(lq),
    ).slice(0, 20)
  }, [threads, q])

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Escape') onClose()
  }

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[15vh]" onClick={onClose}>
      <div
        className="w-full max-w-lg rounded-lg border border-slate-700 bg-slate-900 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <input
          ref={inputRef}
          value={q}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={onKeyDown}
          placeholder="Jump to thread..."
          className="w-full border-b border-slate-700 bg-transparent px-4 py-3 text-sm text-slate-200 placeholder-slate-500 outline-none"
        />
        <div className="max-h-64 overflow-y-auto">
          {filtered.map((t) => (
            <button
              key={t.id}
              onClick={() => onSelect(t.slug)}
              className="flex w-full items-center justify-between px-4 py-2 text-left text-sm text-slate-300 transition hover:bg-slate-800"
            >
              <span className="truncate">{t.title}</span>
              <span className="ml-2 flex-shrink-0 text-[10px] text-slate-600">{t.project}</span>
            </button>
          ))}
          {filtered.length === 0 && (
            <div className="px-4 py-3 text-xs text-slate-600">No matching threads</div>
          )}
        </div>
      </div>
    </div>
  )
}

/* ------------------------------------------------------------------ */
/*  Main Brain Page                                                    */
/* ------------------------------------------------------------------ */

export function Brain() {
  const { threadSlug } = useParams<{ threadSlug: string }>()
  const navigate = useNavigate()

  const [threads, setThreads] = useState<BrainThread[]>([])
  const [threadsLoading, setThreadsLoading] = useState(true)
  const [threadsError, setThreadsError] = useState<string | null>(null)

  const [messages, setMessages] = useState<BrainMessage[]>([])
  const [msgsLoading, setMsgsLoading] = useState(false)
  const [msgsError, setMsgsError] = useState<string | null>(null)

  const [searchQuery, setSearchQuery] = useState('')
  const [showPicker, setShowPicker] = useState(false)

  // Fetch threads
  useEffect(() => {
    let cancelled = false
    async function load() {
      try {
        const data = await getJson<{ threads: BrainThread[] }>('/api/brain/threads')
        if (cancelled) return
        setThreads(data.threads || [])
        setThreadsLoading(false)
      } catch (e) {
        if (cancelled) return
        setThreadsError(e instanceof Error ? e.message : String(e))
        setThreadsLoading(false)
      }
    }
    load()
    const timer = setInterval(load, 15_000)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [])

  // Fetch messages for active thread
  useEffect(() => {
    if (!threadSlug) {
      setMessages([])
      return
    }
    let cancelled = false
    setMsgsLoading(true)
    setMsgsError(null)

    async function load() {
      try {
        const data = await getJson<{ messages: BrainMessage[] }>(
          `/api/brain/threads/${threadSlug}/messages`,
        )
        if (cancelled) return
        setMessages(data.messages || [])
        setMsgsLoading(false)
      } catch (e) {
        if (cancelled) return
        setMsgsError(e instanceof Error ? e.message : String(e))
        setMsgsLoading(false)
      }
    }
    load()
    const timer = setInterval(load, 5_000)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [threadSlug])

  // Cmd+K listener
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
        e.preventDefault()
        setShowPicker((p) => !p)
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [])

  const activeThread = threads.find((t) => t.slug === threadSlug)

  const onPickerSelect = useCallback(
    (slug: string) => {
      setShowPicker(false)
      navigate(`/brain/${slug}`)
    },
    [navigate],
  )

  // Loading / error for threads
  if (threadsLoading) {
    return <div className="p-6 text-slate-400">Loading brain threads...</div>
  }
  if (threadsError && threads.length === 0) {
    return (
      <div className="p-6">
        <div className="text-rose-400">Could not load brain threads: {threadsError}</div>
        <div className="mt-2 text-sm text-slate-500">
          The Brain API is not running yet. Start the daemon or check logs.
        </div>
      </div>
    )
  }

  return (
    <div className="flex h-full -m-4 md:-m-6">
      {showPicker && (
        <FuzzyPicker
          threads={threads}
          onSelect={onPickerSelect}
          onClose={() => setShowPicker(false)}
        />
      )}

      {/* Left: Thread sidebar */}
      <ThreadSidebar
        threads={threads}
        activeSlug={threadSlug}
        searchQuery={searchQuery}
        onSearchChange={setSearchQuery}
      />

      {/* Center: Chat or landing */}
      {threadSlug ? (
        <ChatPanel
          threadSlug={threadSlug}
          messages={messages}
          loading={msgsLoading}
          error={msgsError}
        />
      ) : (
        <div className="flex flex-1 flex-col items-center justify-center text-center">
          <div className="mb-4 text-4xl">🧠</div>
          <h2 className="mb-2 text-xl font-semibold text-slate-200">Virtual Brain</h2>
          <p className="mb-6 max-w-md text-sm text-slate-500">
            Select a thread from the sidebar or create a new one. Press{' '}
            <kbd className="rounded border border-slate-700 bg-slate-800 px-1.5 py-0.5 text-xs text-slate-300">
              Cmd+K
            </kbd>{' '}
            to quickly jump to any thread.
          </p>
          <button
            onClick={() => navigate('/brain/new')}
            className="rounded border border-violet-500 px-6 py-2.5 text-sm font-medium text-violet-300 transition hover:bg-violet-500/20"
          >
            + New Thread
          </button>
        </div>
      )}

      {/* Right: Stack + Backlog */}
      <RightPanel threadSlug={threadSlug} project={activeThread?.project} />
    </div>
  )
}
