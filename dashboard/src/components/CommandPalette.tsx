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
  // Navigation
  { id: 'home', label: 'Home', category: 'Navigate', path: '/' },
  { id: 'fleet', label: 'Fleet Overview', category: 'Navigate', path: '/' },
  { id: 'topology', label: 'Topology', category: 'Navigate', path: '/topology' },
  { id: 'models', label: 'Model Inventory', category: 'Navigate', path: '/models' },
  { id: 'model-hub', label: 'Model Hub', category: 'Navigate', path: '/model-hub' },
  { id: 'tools', label: 'Tool Inventory', category: 'Navigate', path: '/tools' },
  { id: 'metrics', label: 'Metrics', category: 'Navigate', path: '/metrics' },
  { id: 'mission', label: 'Mission Control', category: 'Navigate', path: '/mission-control' },
  { id: 'tasks', label: 'My Tasks', category: 'Navigate', path: '/my-tasks' },
  { id: 'projects', label: 'Projects', category: 'Navigate', path: '/projects' },
  { id: 'planning', label: 'Planning Hub', category: 'Navigate', path: '/planning' },
  { id: 'chat', label: 'Chat Studio', category: 'Navigate', path: '/chat' },
  { id: 'chats', label: 'Chats', category: 'Navigate', path: '/chats' },
  { id: 'workflow', label: 'Workflow Workbench', category: 'Navigate', path: '/workflow' },
  { id: 'settings', label: 'Settings', category: 'Navigate', path: '/settings' },
  { id: 'config', label: 'Config Editor', category: 'Navigate', path: '/config' },
  { id: 'proxy', label: 'LLM Proxy', category: 'Navigate', path: '/llm-proxy' },
  { id: 'audit', label: 'Audit Log', category: 'Navigate', path: '/audit' },
  { id: 'updates', label: 'Updates', category: 'Navigate', path: '/updates' },
  { id: 'onboarding', label: 'Operator Onboarding', category: 'Navigate', path: '/onboarding' },
  // Actions
  { id: 'new-chat', label: 'New Chat', category: 'Action', path: '/chat' },
  { id: 'fleet-health', label: 'Check Fleet Health', category: 'Action', path: '/' },
  { id: 'refresh', label: 'Refresh Page', category: 'Action', action: () => window.location.reload() },
]

export function CommandPalette() {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState(0)
  const navigate = useNavigate()

  // Cmd+K to open
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
      <div className="relative w-full max-w-lg rounded-xl border border-slate-700 bg-slate-900 shadow-2xl shadow-black/50"
           onClick={e => e.stopPropagation()}>
        <div className="flex items-center border-b border-slate-800 px-4">
          <svg className="h-5 w-5 text-slate-500" fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z" />
          </svg>
          <input
            type="text"
            value={query}
            onChange={e => { setQuery(e.target.value); setSelected(0) }}
            onKeyDown={handleKeyDown}
            placeholder="Search commands, pages..."
            className="flex-1 bg-transparent px-3 py-3 text-sm text-slate-100 outline-none placeholder:text-slate-500"
            autoFocus
          />
          <kbd className="rounded border border-slate-700 bg-slate-800 px-1.5 py-0.5 text-xs text-slate-500">esc</kbd>
        </div>

        <div className="max-h-72 overflow-y-auto p-2">
          {filtered.length === 0 ? (
            <div className="px-3 py-6 text-center text-sm text-slate-500">No results for "{query}"</div>
          ) : (
            filtered.map((item, i) => (
              <button
                key={item.id}
                onClick={() => execute(item)}
                onMouseEnter={() => setSelected(i)}
                className={`flex w-full items-center justify-between rounded-lg px-3 py-2 text-sm transition ${
                  i === selected ? 'bg-violet-500/20 text-violet-200' : 'text-slate-300 hover:bg-slate-800'
                }`}
              >
                <div className="flex items-center gap-3">
                  <span className="text-xs text-slate-500 w-16">{item.category}</span>
                  <span>{item.label}</span>
                </div>
                {item.shortcut && <kbd className="text-xs text-slate-600">{item.shortcut}</kbd>}
              </button>
            ))
          )}
        </div>

        <div className="border-t border-slate-800 px-4 py-2 text-xs text-slate-500">
          ↑↓ navigate • ↵ select • esc close
        </div>
      </div>
    </div>
  )
}
