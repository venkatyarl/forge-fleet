import { useCallback, useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { getJson } from '../lib/api'
import { extractNodes, extractSummary } from '../lib/normalizers'
import type { FleetNode, FleetStatusResponse } from '../types'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type TaskSummary = {
  id: string
  title: string
  status: string
  priority?: string | number
  assignee?: string
}

type AuditEntry = {
  id?: string
  event_type: string
  actor?: string
  details_json?: string
  timestamp?: string
  created_at?: string
}

type ChatSummary = {
  id: string
  title?: string
  model?: string
  created_at?: string
  message_count?: number
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function timeAgo(ts: string | undefined): string {
  if (!ts) return ''
  const diff = Date.now() - new Date(ts).getTime()
  const mins = Math.floor(diff / 60_000)
  if (mins < 1) return 'just now'
  if (mins < 60) return `${mins}m ago`
  const hours = Math.floor(mins / 60)
  if (hours < 24) return `${hours}h ago`
  return `${Math.floor(hours / 24)}d ago`
}

function statusColor(status: string): string {
  switch (status.toLowerCase().replace(/-/g, '_')) {
    case 'in_progress': return 'bg-blue-500/20 text-blue-300 border-blue-500/30'
    case 'todo': return 'bg-amber-500/20 text-amber-300 border-amber-500/30'
    case 'done': return 'bg-emerald-500/20 text-emerald-300 border-emerald-500/30'
    case 'blocked': return 'bg-rose-500/20 text-rose-300 border-rose-500/30'
    case 'review': return 'bg-purple-500/20 text-purple-300 border-purple-500/30'
    default: return 'bg-zinc-500/20 text-zinc-300 border-zinc-500/30'
  }
}

// ---------------------------------------------------------------------------
// Sub-components
// ---------------------------------------------------------------------------

function StatCard({ label, value, sub, color }: { label: string; value: string | number; sub?: string; color?: string }) {
  return (
    <div className="rounded-xl border border-zinc-800 bg-zinc-900/70 p-4">
      <p className="text-xs font-medium uppercase tracking-wider text-zinc-500">{label}</p>
      <p className={`mt-1 text-2xl font-bold ${color ?? 'text-zinc-100'}`}>{value}</p>
      {sub && <p className="mt-0.5 text-xs text-zinc-500">{sub}</p>}
    </div>
  )
}

function QuickAction({ label, icon, to }: { label: string; icon: string; to: string }) {
  return (
    <Link
      to={to}
      className="flex items-center gap-2 rounded-lg border border-zinc-800 bg-zinc-900/50 px-4 py-3 text-sm text-zinc-300 transition hover:border-violet-500/40 hover:bg-violet-500/10 hover:text-violet-200"
    >
      <span>{icon}</span>
      <span>{label}</span>
    </Link>
  )
}

// ---------------------------------------------------------------------------
// Main component
// ---------------------------------------------------------------------------

export function MissionControl() {
  const [nodes, setNodes] = useState<FleetNode[]>([])
  const [summary, setSummary] = useState<{ total_nodes?: number; connected_nodes?: number; unhealthy_nodes?: number; model_count?: number; leader?: string }>({})
  const [tasks, setTasks] = useState<TaskSummary[]>([])
  const [activity, setActivity] = useState<AuditEntry[]>([])
  const [chats, setChats] = useState<ChatSummary[]>([])
  const [loading, setLoading] = useState(true)

  const load = useCallback(async () => {
    setLoading(true)
    try {
      // Load everything in parallel
      const [fleetData, taskData, auditData, chatData] = await Promise.allSettled([
        getJson<FleetStatusResponse>('/api/fleet/status').catch(() => getJson<FleetStatusResponse>('/api/status')),
        getJson<TaskSummary[]>('/api/mc/work-items').catch(() => []),
        getJson<AuditEntry[]>('/api/audit?limit=10').catch(() => []),
        getJson<ChatSummary[]>('/api/agent/sessions').catch(() => []),
      ])

      if (fleetData.status === 'fulfilled' && fleetData.value) {
        setNodes(extractNodes(fleetData.value))
        setSummary(extractSummary(fleetData.value))
      }
      if (taskData.status === 'fulfilled' && Array.isArray(taskData.value)) {
        setTasks(taskData.value.slice(0, 8))
      }
      if (auditData.status === 'fulfilled' && Array.isArray(auditData.value)) {
        setActivity(auditData.value.slice(0, 8))
      }
      if (chatData.status === 'fulfilled' && Array.isArray(chatData.value)) {
        setChats(chatData.value.slice(0, 5))
      }
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => { void load() }, [load])

  const onlineNodes = nodes.filter(n => n.status === 'online' || n.status === 'connected')
  const healthPct = nodes.length > 0 ? Math.round((onlineNodes.length / nodes.length) * 100) : 0
  const activeTasks = tasks.filter(t => t.status === 'in_progress')
  const pendingTasks = tasks.filter(t => t.status === 'todo' || t.status === 'backlog')
  const modelCount = summary.model_count ?? nodes.reduce((sum, n) => sum + (n.models?.length ?? 0), 0)

  return (
    <section className="space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-zinc-100">Mission Control</h1>
          <p className="text-sm text-zinc-500">ForgeFleet command center</p>
        </div>
        <button
          onClick={() => void load()}
          className="rounded-md border border-zinc-700 bg-zinc-900 px-3 py-1.5 text-sm text-zinc-300 hover:border-zinc-500 transition"
          type="button"
        >
          Refresh
        </button>
      </div>

      {/* Stat cards row */}
      {loading ? (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          {[1,2,3,4].map(i => (
            <div key={i} className="h-24 animate-pulse rounded-xl border border-zinc-800 bg-zinc-900/50" />
          ))}
        </div>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <StatCard
            label="Fleet Health"
            value={`${healthPct}%`}
            sub={`${onlineNodes.length}/${nodes.length} nodes online`}
            color={healthPct === 100 ? 'text-emerald-400' : healthPct >= 50 ? 'text-amber-400' : 'text-rose-400'}
          />
          <StatCard
            label="Active Tasks"
            value={activeTasks.length}
            sub={`${pendingTasks.length} pending`}
            color="text-blue-400"
          />
          <StatCard
            label="Models Loaded"
            value={modelCount}
            sub={`across ${nodes.length} nodes`}
          />
          <StatCard
            label="Recent Chats"
            value={chats.length}
            sub="sessions"
            color="text-violet-400"
          />
        </div>
      )}

      {/* Main grid: 2 columns on large screens */}
      <div className="grid gap-6 lg:grid-cols-3">

        {/* Left column (2/3) */}
        <div className="lg:col-span-2 space-y-6">

          {/* Fleet Nodes */}
          <div className="rounded-xl border border-zinc-800 bg-zinc-900/50 p-4">
            <div className="mb-3 flex items-center justify-between">
              <h2 className="text-sm font-semibold uppercase tracking-wider text-zinc-400">Fleet Nodes</h2>
              <Link to="/fleet" className="text-xs text-violet-400 hover:text-violet-300">View all</Link>
            </div>
            {nodes.length === 0 && !loading ? (
              <p className="text-sm text-zinc-500">No fleet data available</p>
            ) : (
              <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-3">
                {nodes.slice(0, 6).map(node => (
                  <Link
                    key={node.name}
                    to={`/nodes/${node.name}`}
                    className="flex items-center gap-3 rounded-lg border border-zinc-800 bg-zinc-950/50 p-3 transition hover:border-zinc-700"
                  >
                    <span className={`h-2 w-2 rounded-full flex-shrink-0 ${
                      node.status === 'online' || node.status === 'connected' ? 'bg-emerald-400' : 'bg-zinc-600'
                    }`} />
                    <div className="min-w-0">
                      <p className="text-sm font-medium text-zinc-200 truncate capitalize">{node.name}</p>
                      <p className="text-xs text-zinc-500 truncate">{node.hostname ?? node.ip ?? '—'}</p>
                    </div>
                  </Link>
                ))}
              </div>
            )}
          </div>

          {/* Active Tasks */}
          <div className="rounded-xl border border-zinc-800 bg-zinc-900/50 p-4">
            <div className="mb-3 flex items-center justify-between">
              <h2 className="text-sm font-semibold uppercase tracking-wider text-zinc-400">Tasks</h2>
              <Link to="/my-tasks" className="text-xs text-violet-400 hover:text-violet-300">View all</Link>
            </div>
            {tasks.length === 0 ? (
              <p className="text-sm text-zinc-500">No tasks yet</p>
            ) : (
              <div className="space-y-2">
                {tasks.slice(0, 6).map(task => (
                  <div key={task.id} className="flex items-center gap-3 rounded-lg border border-zinc-800 bg-zinc-950/50 p-3">
                    <span className={`rounded-full border px-2 py-0.5 text-[10px] font-medium ${statusColor(task.status)}`}>
                      {task.status.replace(/_/g, ' ')}
                    </span>
                    <p className="flex-1 text-sm text-zinc-200 truncate">{task.title}</p>
                    {task.assignee && <span className="text-xs text-zinc-500">{task.assignee}</span>}
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>

        {/* Right column (1/3) */}
        <div className="space-y-6">

          {/* Quick Actions */}
          <div className="rounded-xl border border-zinc-800 bg-zinc-900/50 p-4">
            <h2 className="mb-3 text-sm font-semibold uppercase tracking-wider text-zinc-400">Quick Actions</h2>
            <div className="space-y-2">
              <QuickAction icon="✨" label="New Chat" to="/chat" />
              <QuickAction icon="📊" label="Fleet Overview" to="/fleet" />
              <QuickAction icon="📁" label="Projects" to="/projects" />
              <QuickAction icon="🔄" label="Workflows" to="/workflow" />
            </div>
          </div>

          {/* Recent Chats */}
          <div className="rounded-xl border border-zinc-800 bg-zinc-900/50 p-4">
            <div className="mb-3 flex items-center justify-between">
              <h2 className="text-sm font-semibold uppercase tracking-wider text-zinc-400">Recent Chats</h2>
              <Link to="/chats" className="text-xs text-violet-400 hover:text-violet-300">View all</Link>
            </div>
            {chats.length === 0 ? (
              <p className="text-sm text-zinc-500">No chats yet</p>
            ) : (
              <div className="space-y-2">
                {chats.map(chat => (
                  <Link
                    key={chat.id}
                    to={`/chat/${chat.id}`}
                    className="block rounded-lg border border-zinc-800 bg-zinc-950/50 p-3 transition hover:border-zinc-700"
                  >
                    <p className="text-sm text-zinc-200 truncate">{chat.title || 'Untitled'}</p>
                    <div className="mt-1 flex items-center gap-2 text-xs text-zinc-500">
                      {chat.model && <span>{chat.model}</span>}
                      {chat.created_at && <span>{timeAgo(chat.created_at)}</span>}
                    </div>
                  </Link>
                ))}
              </div>
            )}
          </div>

          {/* Activity Feed */}
          <div className="rounded-xl border border-zinc-800 bg-zinc-900/50 p-4">
            <div className="mb-3 flex items-center justify-between">
              <h2 className="text-sm font-semibold uppercase tracking-wider text-zinc-400">Activity</h2>
              <Link to="/audit" className="text-xs text-violet-400 hover:text-violet-300">View all</Link>
            </div>
            {activity.length === 0 ? (
              <p className="text-sm text-zinc-500">No recent activity</p>
            ) : (
              <div className="space-y-2">
                {activity.map((evt, i) => (
                  <div key={evt.id ?? i} className="rounded-lg border border-zinc-800 bg-zinc-950/50 p-2">
                    <p className="text-xs text-zinc-300 truncate">
                      {evt.event_type.replace(/_/g, ' ')}
                    </p>
                    <div className="mt-0.5 flex items-center gap-2 text-[10px] text-zinc-500">
                      {evt.actor && <span>{evt.actor}</span>}
                      <span>{timeAgo(evt.timestamp ?? evt.created_at)}</span>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      </div>
    </section>
  )
}
