import { useCallback, useEffect, useMemo, useState } from 'react'
import { useNavigate } from 'react-router-dom'

type CommandItem = {
  id: string
  label: string
  category: string
  path?: string
  action?: () => void
  shortcut?: string
}

const COMMANDS: CommandItem[] = [
  // Mission Control
  { id: 'home', label: 'Mission Control', category: 'Navigate', path: '/', shortcut: 'G H' },
  // Chats
  { id: 'chat', label: 'New Chat', category: 'Chats', path: '/chat', shortcut: 'G C' },
  { id: 'chats', label: 'Chat History', category: 'Chats', path: '/chats' },
  // Project Management
  { id: 'tasks', label: 'My Tasks', category: 'Projects', path: '/my-tasks' },
  { id: 'projects', label: 'Projects', category: 'Projects', path: '/projects', shortcut: 'G P' },
  { id: 'planning', label: 'Planning Hub', category: 'Projects', path: '/planning' },
  { id: 'workflow', label: 'Workflows', category: 'Projects', path: '/workflow' },
  // Fleet (via Settings)
  { id: 'fleet', label: 'Fleet Overview', category: 'Fleet', path: '/fleet', shortcut: 'G F' },
  { id: 'fleet-members', label: 'Fleet Members', category: 'Settings', path: '/settings#fleet' },
  { id: 'topology', label: 'Topology', category: 'Fleet', path: '/topology', shortcut: 'G T' },
  { id: 'model-hub', label: 'Available Models', category: 'Fleet', path: '/model-hub' },
  { id: 'tools', label: 'Tools', category: 'Fleet', path: '/tools', shortcut: 'G K' },
  { id: 'metrics', label: 'Metrics', category: 'Fleet', path: '/metrics' },
  { id: 'onboarding', label: 'Add New Fleet Member', category: 'Fleet', path: '/onboarding' },
  // Settings
  { id: 'settings', label: 'Settings', category: 'Admin', path: '/settings', shortcut: 'G S' },
  { id: 'proxy', label: 'LLM Proxy', category: 'Admin', path: '/llm-proxy' },
  { id: 'audit', label: 'Audit Log', category: 'Admin', path: '/audit' },
  { id: 'updates', label: 'Updates', category: 'Admin', path: '/updates' },
  // Actions
  { id: 'new-chat', label: 'Start New Chat', category: 'Action', path: '/chat' },
  { id: 'fleet-health', label: 'Check Fleet Health', category: 'Action', path: '/fleet' },
  { id: 'refresh', label: 'Refresh Page', category: 'Action', action: () => window.location.reload() },
]

export function CommandPalette() {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState(0)
  const navigate = useNavigate()

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
        e.preventDefault()
        setOpen(prev => !prev)
        setQuery('')
        setSelected(0)
      }
      if (e.key === 'Escape') setOpen(false)
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [])

  const filtered = useMemo(() => {
    if (!query) return COMMANDS
    const lower = query.toLowerCase()
    return COMMANDS.filter(c =>
      c.label.toLowerCase().includes(lower) ||
      c.category.toLowerCase().includes(lower) ||
      c.id.includes(lower)
    )
  }, [query])

  const execute = useCallback((item: CommandItem) => {
    if (item.path) navigate(item.path)
    if (item.action) item.action()
    setOpen(false)
    setQuery('')
  }, [navigate])

  const handleKeyDown = useCallback((e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') { e.preventDefault(); setSelected(s => Math.min(s + 1, filtered.length - 1)) }
    if (e.key === 'ArrowUp') { e.preventDefault(); setSelected(s => Math.max(s - 1, 0)) }
    if (e.key === 'Enter' && filtered[selected]) { execute(filtered[selected]) }
  }, [filtered, selected, execute])

  if (!open) return null

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[20vh]" onClick={() => setOpen(false)}>
      <div className="fixed inset-0 bg-black/60" />
      <div className="relative w-full max-w-lg rounded-xl border border-zinc-700 bg-zinc-900 shadow-2xl shadow-black/50"
           onClick={e => e.stopPropagation()}>
        <div className="flex items-center border-b border-zinc-800 px-4">
          <svg className="h-5 w-5 text-zinc-500" fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z" />
          </svg>
          <input
            type="text"
            value={query}
            onChange={e => { setQuery(e.target.value); setSelected(0) }}
            onKeyDown={handleKeyDown}
            placeholder="Search commands, pages..."
            className="flex-1 bg-transparent px-3 py-3 text-sm text-zinc-100 outline-none placeholder:text-zinc-500"
            autoFocus
          />
          <kbd className="rounded border border-zinc-700 bg-zinc-800 px-1.5 py-0.5 text-xs text-zinc-500">esc</kbd>
        </div>

        <div className="max-h-72 overflow-y-auto p-2">
          {filtered.length === 0 ? (
            <div className="px-3 py-6 text-center text-sm text-zinc-500">No results for "{query}"</div>
          ) : (
            filtered.map((item, i) => (
              <button
                key={item.id}
                onClick={() => execute(item)}
                onMouseEnter={() => setSelected(i)}
                className={`flex w-full items-center justify-between rounded-lg px-3 py-2 text-sm transition ${
                  i === selected ? 'bg-violet-500/20 text-violet-200' : 'text-zinc-300 hover:bg-zinc-800'
                }`}
              >
                <div className="flex items-center gap-3">
                  <span className="text-xs text-zinc-500 w-16">{item.category}</span>
                  <span>{item.label}</span>
                </div>
                {item.shortcut && <kbd className="text-xs text-zinc-600">{item.shortcut}</kbd>}
              </button>
            ))
          )}
        </div>

        <div className="border-t border-zinc-800 px-4 py-2 text-xs text-zinc-500">
          ↑↓ navigate &middot; ↵ select &middot; esc close
        </div>
      </div>
    </div>
  )
}
