import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useNavigate, useParams } from 'react-router-dom'
import {
  BrainCircuit,
  ChevronDown,
  Inbox,
  Layers,
  MessageSquare,
  Plus,
  Search,
  Send,
} from 'lucide-react'
import { getJson, postJson } from '../lib/api'
import { useWsFeed } from '../hooks/useWsFeed'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { Badge } from '../components/ui/badge'
import { StatusBadge } from '../components/ui/status-badge'
import { Button } from '../components/ui/button'
import { cn, formatElapsed } from '../lib/utils'

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
  const secs = Math.max(0, Math.floor(diff / 1000))
  if (secs < 60) return 'now'
  return formatElapsed(secs).replace(/\s+/g, '')
}

function timeAgo(iso: string): string {
  const label = relTime(iso)
  return label === 'now' ? 'just now' : `${label} ago`
}

function priorityVariant(priority: BacklogItem['priority']): 'crit' | 'warn' | 'info' | 'neutral' {
  if (priority === 'urgent') return 'crit'
  if (priority === 'high') return 'warn'
  if (priority === 'medium') return 'info'
  return 'neutral'
}

function messageTone(role: BrainMessage['role']): string {
  if (role === 'user') return 'ml-auto border-primary/30 bg-primary-subtle text-foreground'
  if (role === 'assistant') return 'border-border bg-panel text-foreground'
  return 'border-border-subtle bg-surface text-muted italic'
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
    <aside className="flex h-full w-72 flex-shrink-0 flex-col border-r border-border bg-surface">
      <div className="border-b border-border p-3">
        <CardHeader className="mb-3">
          <div>
            <CardTitle className="flex items-center gap-2">
              <BrainCircuit className="h-4 w-4 text-primary" />
              Brain
            </CardTitle>
            <CardDescription>{threads.length} threads indexed</CardDescription>
          </div>
          <Badge variant="info">Cmd+K</Badge>
        </CardHeader>
        <label className="relative block">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-dim" />
          <input
            type="text"
            value={searchQuery}
            onChange={(e) => onSearchChange(e.target.value)}
            placeholder="Search threads..."
            className="h-9 w-full rounded-lg border border-border bg-panel pl-8 pr-3 text-sm text-foreground placeholder:text-dim outline-none transition focus:border-primary"
          />
        </label>
      </div>
      <div className="flex-1 overflow-y-auto p-2">
        {Array.from(grouped.entries()).map(([project, items]) => (
          <div key={project} className="mb-3">
            <button
              onClick={() => setCollapsed((p) => ({ ...p, [project]: !p[project] }))}
              className="flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left text-2xs font-semibold uppercase text-dim transition hover:bg-panel hover:text-muted"
            >
              <ChevronDown
                className={cn('h-3 w-3 transition-transform', collapsed[project] && '-rotate-90')}
              />
              <span className="min-w-0 flex-1 truncate">{project}</span>
              <Badge variant="neutral">{items.length}</Badge>
            </button>
            {!collapsed[project] &&
              items.map((t) => {
                const active = activeSlug === t.slug
                return (
                  <button
                    key={t.id}
                    onClick={() => navigate(`/brain/${t.slug}`)}
                    className={cn(
                      'mt-1 flex w-full items-center gap-2 rounded-lg border px-3 py-2 text-left transition',
                      active
                        ? 'border-primary/40 bg-primary-subtle text-foreground'
                        : 'border-transparent text-muted hover:border-border hover:bg-panel hover:text-foreground',
                    )}
                  >
                    <MessageSquare className="h-3.5 w-3.5 flex-shrink-0 text-dim" />
                    <span className="min-w-0 flex-1 truncate text-sm">{t.title}</span>
                    <StatusBadge status={t.status} />
                    <span className="flex-shrink-0 text-2xs text-dim">
                      {relTime(t.last_message_at)}
                    </span>
                  </button>
                )
              })}
          </div>
        ))}
        {grouped.size === 0 && (
          <Card className="border-border bg-panel p-3">
            <CardDescription>No threads found</CardDescription>
          </Card>
        )}
      </div>
    </aside>
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
    <main className="flex min-w-0 flex-1 flex-col bg-background">
      <div className="flex items-center justify-between border-b border-border bg-background px-5 py-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <CardTitle className="truncate">Thread: {threadSlug}</CardTitle>
            <StatusBadge status={loading ? 'running' : error ? 'error' : 'active'} />
          </div>
          <CardDescription>Enter sends, Shift+Enter adds a new line</CardDescription>
        </div>
      </div>

      <div className="flex-1 space-y-3 overflow-y-auto px-5 py-4">
        {loading && <div className="text-sm text-dim">Loading messages...</div>}
        {error && <div className="text-sm text-status-crit">Error: {error}</div>}
        {!loading && !error && messages.length === 0 && (
          <Card className="bg-panel">
            <CardDescription>No messages yet. Start the conversation below.</CardDescription>
          </Card>
        )}
        {messages.map((m) => (
          <div
            key={m.id}
            className={cn(
              'max-w-[82%] rounded-xl border px-3 py-2 text-sm shadow-sm',
              messageTone(m.role),
            )}
          >
            <div className="mb-1 flex items-center gap-1.5 text-2xs font-semibold uppercase text-dim">
              {m.role}
              {m.channel && <Badge variant="neutral">via {m.channel}</Badge>}
            </div>
            <div className="whitespace-pre-wrap leading-relaxed">{m.content}</div>
          </div>
        ))}
        <div ref={bottomRef} />
      </div>

      <div className="border-t border-border bg-surface p-3">
        <div className="flex gap-2 rounded-xl border border-border bg-panel p-2">
          <textarea
            aria-label="Message input"
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={onKeyDown}
            placeholder="Send a message..."
            rows={1}
            className="min-h-9 flex-1 resize-none bg-transparent px-2 py-2 text-sm text-foreground placeholder:text-dim outline-none"
          />
          <Button onClick={send} disabled={sending || !input.trim()} className="self-end">
            <Send className="h-4 w-4" />
            {sending ? 'Sending' : 'Send'}
          </Button>
        </div>
      </div>
    </main>
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
    <aside className="flex h-full w-80 flex-shrink-0 flex-col gap-3 overflow-y-auto border-l border-border bg-surface p-3">
      <Card className="bg-panel">
        <CardHeader>
          <div>
            <CardTitle className="flex items-center gap-2">
              <Layers className="h-4 w-4 text-primary" />
              Stack
            </CardTitle>
            <CardDescription>{threadSlug || 'No active thread'}</CardDescription>
          </div>
          <Badge variant="neutral">{stackItems.length}</Badge>
        </CardHeader>
        {stackItems.length === 0 ? (
          <CardDescription>No stack items</CardDescription>
        ) : (
          <ul className="space-y-2">
            {stackItems.map((it, i) => {
              const pct = it.progress == null ? null : Math.round(it.progress * 100)
              return (
                <li key={i} className="rounded-lg border border-border bg-surface p-2">
                  <div className="flex items-start gap-2 text-sm text-foreground">
                    <span className="mt-1 h-1.5 w-1.5 flex-shrink-0 rounded-full bg-primary" />
                    <span className="min-w-0 flex-1 truncate">{it.title}</span>
                  </div>
                  {it.context && <p className="mt-1 truncate text-xs text-dim">{it.context}</p>}
                  {pct != null && (
                    <div className="mt-2 flex items-center gap-2">
                      <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-elevated">
                        <div className="h-full rounded-full bg-primary" style={{ width: `${pct}%` }} />
                      </div>
                      <span className="text-2xs text-dim">{pct}%</span>
                    </div>
                  )}
                  <p className="mt-1 text-2xs text-dim">{timeAgo(it.pushed_at)}</p>
                </li>
              )
            })}
          </ul>
        )}
      </Card>

      <Card className="bg-panel">
        <CardHeader>
          <div>
            <CardTitle className="flex items-center gap-2">
              <Inbox className="h-4 w-4 text-primary" />
              Backlog
            </CardTitle>
            <CardDescription>{project || 'No project selected'}</CardDescription>
          </div>
          <Badge variant="neutral">{backlogItems.length}</Badge>
        </CardHeader>
        {backlogItems.length === 0 ? (
          <CardDescription>No backlog items</CardDescription>
        ) : (
          <ul className="space-y-2">
            {backlogItems.map((it, i) => (
              <li
                key={i}
                className="flex items-center gap-2 rounded-lg border border-border bg-surface p-2"
              >
                <Badge variant={priorityVariant(it.priority)}>{it.priority}</Badge>
                <span className="min-w-0 flex-1 truncate text-sm text-foreground">{it.title}</span>
              </li>
            ))}
          </ul>
        )}
      </Card>
    </aside>
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
    return threads
      .filter(
        (t) =>
          t.title.toLowerCase().includes(lq) ||
          t.slug.toLowerCase().includes(lq) ||
          t.project.toLowerCase().includes(lq),
      )
      .slice(0, 20)
  }, [threads, q])

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Escape') onClose()
  }

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center bg-background/70 pt-[15vh] backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        className="w-full max-w-xl rounded-xl border border-border bg-panel shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <label className="relative block border-b border-border">
          <Search className="pointer-events-none absolute left-4 top-1/2 h-4 w-4 -translate-y-1/2 text-dim" />
          <input
            ref={inputRef}
            value={q}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={onKeyDown}
            placeholder="Jump to thread..."
            className="w-full bg-transparent py-3 pl-10 pr-4 text-sm text-foreground placeholder:text-dim outline-none"
          />
        </label>
        <div className="max-h-64 overflow-y-auto">
          {filtered.map((t) => (
            <button
              key={t.id}
              onClick={() => onSelect(t.slug)}
              className="flex w-full items-center gap-3 px-4 py-2.5 text-left text-sm text-muted transition hover:bg-surface hover:text-foreground"
            >
              <MessageSquare className="h-3.5 w-3.5 flex-shrink-0 text-dim" />
              <span className="min-w-0 flex-1 truncate">{t.title}</span>
              <Badge variant="neutral">{t.project}</Badge>
            </button>
          ))}
          {filtered.length === 0 && (
            <div className="px-4 py-3 text-xs text-dim">No matching threads</div>
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

  // WebSocket feed for real-time message push
  const { lastEvent } = useWsFeed('/ws')

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
    // Fallback polling every 30s; primary updates come via WebSocket.
    const timer = setInterval(load, 30_000)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [threadSlug])

  // React to WebSocket message events for the current thread
  useEffect(() => {
    if (!lastEvent || lastEvent.type !== 'message') return
    const payload = lastEvent.payload as Record<string, unknown> | undefined
    if (payload && payload.thread_slug === threadSlug) {
      // Refresh messages when a new message arrives for this thread
      setMsgsLoading(true)
      getJson<{ messages: BrainMessage[] }>(`/api/brain/threads/${threadSlug}/messages`)
        .then((d) => {
          setMessages(d.messages || [])
          setMsgsLoading(false)
        })
        .catch(() => setMsgsLoading(false))
    }
  }, [lastEvent, threadSlug])

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
    return <div className="p-6 text-muted">Loading brain threads...</div>
  }
  if (threadsError && threads.length === 0) {
    return (
      <div className="p-6">
        <div className="text-status-crit">Could not load brain threads: {threadsError}</div>
        <div className="mt-2 text-sm text-dim">
          The Brain API is not running yet. Start the daemon or check logs.
        </div>
      </div>
    )
  }

  return (
    <div className="-m-4 flex h-full min-h-[calc(100vh-4rem)] overflow-hidden bg-background md:-m-6">
      {showPicker && (
        <FuzzyPicker
          threads={threads}
          onSelect={onPickerSelect}
          onClose={() => setShowPicker(false)}
        />
      )}

      <ThreadSidebar
        threads={threads}
        activeSlug={threadSlug}
        searchQuery={searchQuery}
        onSearchChange={setSearchQuery}
      />

      {threadSlug ? (
        <ChatPanel
          threadSlug={threadSlug}
          messages={messages}
          loading={msgsLoading}
          error={msgsError}
        />
      ) : (
        <main className="flex flex-1 flex-col items-center justify-center bg-background p-6 text-center">
          <Card className="max-w-md bg-panel">
            <CardHeader className="justify-center">
              <div className="flex flex-col items-center gap-3">
                <div className="flex h-12 w-12 items-center justify-center rounded-xl border border-primary/30 bg-primary-subtle text-primary">
                  <BrainCircuit className="h-6 w-6" />
                </div>
                <div>
                  <CardTitle className="text-lg">Virtual Brain</CardTitle>
                  <CardDescription className="mt-1">
                    Select a thread from the sidebar or press Cmd+K to jump directly.
                  </CardDescription>
                </div>
              </div>
            </CardHeader>
            <Button onClick={() => navigate('/brain/new')} className="mt-2">
              <Plus className="h-4 w-4" />
              New Thread
            </Button>
          </Card>
        </main>
      )}

      <RightPanel threadSlug={threadSlug} project={activeThread?.project} />
    </div>
  )
}
