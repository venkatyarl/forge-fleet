import { useCallback, useEffect, useState } from 'react'
import { postJson, getJson } from '../lib/api'

type ChatEntry = {
  id: string
  name: string
  scope_display: string
  status: string
  last_active_at: string
  message_count: number
  preview: string
  stack_depth: number
  backlog_count: number
}

export function Chats() {
  const [chats, setChats] = useState<ChatEntry[]>([])
  const [folders, setFolders] = useState<string[]>([])
  const [selectedFolder, setSelectedFolder] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [filter, setFilter] = useState<string>('all')

  const loadChats = useCallback(async () => {
    setLoading(true)
    try {
      const data = await getJson<{ chats: ChatEntry[] }>('/api/chats')
      setChats(data.chats || [])
      const folderData = await getJson<{ folders: string[] }>('/api/chats/folders')
      setFolders(folderData.folders || [])
    } catch {
      // Fallback: no chats yet
      setChats([])
    }
    setLoading(false)
  }, [])

  useEffect(() => { void loadChats() }, [loadChats])

  const createChat = async (scope: string) => {
    try {
      const result = await postJson<{ chat: ChatEntry }>('/api/chats', { scope })
      if (result?.chat) {
        setChats(prev => [result.chat!, ...prev])
      }
    } catch {
      // handle error
    }
  }

  const filteredChats = chats.filter(c => {
    if (filter === 'all') return true
    if (filter === 'active') return c.status === 'active'
    if (filter === 'project') return c.scope_display.startsWith('Project:')
    if (filter === 'global') return c.scope_display === 'ForgeFleet (Global)'
    return true
  })

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-semibold text-slate-100">Chats</h2>
          <p className="text-sm text-slate-400">All conversations across projects and scopes</p>
        </div>
        <div className="flex gap-2">
          <button
            onClick={() => void createChat('global')}
            className="rounded-md border border-violet-500/40 bg-violet-500/20 px-3 py-1.5 text-sm text-violet-200 hover:bg-violet-500/30"
          >
            New Chat
          </button>
        </div>
      </div>

      {/* Filters */}
      <div className="flex gap-2">
        {['all', 'active', 'project', 'global'].map(f => (
          <button
            key={f}
            onClick={() => setFilter(f)}
            className={`rounded-full px-3 py-1 text-xs ${
              filter === f
                ? 'bg-violet-500/30 text-violet-200 border border-violet-500/50'
                : 'bg-slate-800 text-slate-400 border border-slate-700 hover:border-slate-600'
            }`}
          >
            {f.charAt(0).toUpperCase() + f.slice(1)}
          </button>
        ))}
      </div>

      {/* Folder sidebar + chat list */}
      <div className="grid grid-cols-4 gap-4">
        {/* Folders */}
        <div className="col-span-1 rounded-xl border border-slate-800 bg-slate-900/70 p-3">
          <h3 className="mb-2 text-sm font-medium text-slate-300">Folders</h3>
          <div className="space-y-1">
            <button
              onClick={() => setSelectedFolder(null)}
              className={`w-full rounded px-2 py-1 text-left text-sm ${
                selectedFolder === null ? 'bg-violet-500/20 text-violet-200' : 'text-slate-400 hover:bg-slate-800'
              }`}
            >
              All Chats
            </button>
            {folders.map(folder => (
              <button
                key={folder}
                onClick={() => setSelectedFolder(folder)}
                className={`w-full rounded px-2 py-1 text-left text-sm ${
                  selectedFolder === folder ? 'bg-violet-500/20 text-violet-200' : 'text-slate-400 hover:bg-slate-800'
                }`}
              >
                📁 {folder}
              </button>
            ))}
          </div>
        </div>

        {/* Chat list */}
        <div className="col-span-3 space-y-2">
          {loading ? (
            <div className="rounded-xl border border-slate-800 bg-slate-900/70 p-8 text-center text-slate-500">
              Loading chats...
            </div>
          ) : filteredChats.length === 0 ? (
            <div className="rounded-xl border border-slate-800 bg-slate-900/70 p-8 text-center">
              <p className="text-slate-400">No chats yet</p>
              <p className="mt-1 text-sm text-slate-500">Start a new chat or open one from a project</p>
            </div>
          ) : (
            filteredChats.map(chat => (
              <article
                key={chat.id}
                className="rounded-xl border border-slate-800 bg-slate-900/70 p-4 hover:border-slate-700 cursor-pointer transition"
              >
                <div className="flex items-start justify-between">
                  <div className="flex-1 min-w-0">
                    <div className="flex items-center gap-2">
                      <h3 className="font-medium text-slate-200 truncate">{chat.name}</h3>
                      <span className={`rounded-full px-2 py-0.5 text-xs ${
                        chat.status === 'active' ? 'bg-emerald-500/20 text-emerald-300' :
                        chat.status === 'paused' ? 'bg-amber-500/20 text-amber-300' :
                        'bg-slate-700 text-slate-400'
                      }`}>
                        {chat.status}
                      </span>
                    </div>
                    <p className="mt-1 text-sm text-slate-400 truncate">{chat.preview || 'No messages yet'}</p>
                    <div className="mt-2 flex items-center gap-3 text-xs text-slate-500">
                      <span>{chat.scope_display}</span>
                      <span>{chat.message_count} messages</span>
                      <span>{new Date(chat.last_active_at).toLocaleDateString()}</span>
                    </div>
                  </div>
                  <div className="flex flex-col items-end gap-1 ml-4">
                    {chat.stack_depth > 0 && (
                      <span className="rounded bg-amber-500/20 px-2 py-0.5 text-xs text-amber-300">
                        Stack: {chat.stack_depth}
                      </span>
                    )}
                    {chat.backlog_count > 0 && (
                      <span className="rounded bg-sky-500/20 px-2 py-0.5 text-xs text-sky-300">
                        Backlog: {chat.backlog_count}
                      </span>
                    )}
                  </div>
                </div>
              </article>
            ))
          )}
        </div>
      </div>
    </section>
  )
}
