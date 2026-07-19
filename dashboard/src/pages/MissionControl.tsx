import { Link } from 'react-router-dom'
import { Activity, MessageSquare, Rocket, LayoutGrid } from 'lucide-react'
import { Card, CardHeader, CardTitle, CardDescription } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { WorkQueueMetrics } from '../components/work_queue_metrics'
import { useFleetStatus, useWorkItems, useAgentSessions, useAuditRecent } from '../features/hooks/useDashboardQueries'
import { formatElapsed } from '../lib/utils'

function timeAgo(ts: string | undefined): string {
  if (!ts) return ''
  const diff = Date.now() - new Date(ts).getTime()
  const secs = Math.floor(diff / 1000)
  if (secs < 60) return 'just now'
  return formatElapsed(secs) + ' ago'
}

function StatCard({
  label,
  value,
  sub,
  tone,
  icon: Icon,
}: {
  label: string
  value: string | number
  sub?: string
  tone?: 'ok' | 'warn' | 'crit' | 'info'
  icon: React.ElementType
}) {
  const toneClass =
    tone === 'ok'
      ? 'text-status-ok'
      : tone === 'warn'
      ? 'text-status-warn'
      : tone === 'crit'
      ? 'text-status-crit'
      : tone === 'info'
      ? 'text-status-info'
      : 'text-foreground'

  return (
    <Card>
      <CardHeader className="mb-2">
        <CardDescription className="flex items-center gap-1.5">
          <Icon className="h-3.5 w-3.5" />
          {label}
        </CardDescription>
      </CardHeader>
      <div className={`text-2xl font-bold ${toneClass}`}>{value}</div>
      {sub && <div className="mt-0.5 text-xs text-dim">{sub}</div>}
    </Card>
  )
}

function QuickAction({ label, icon: Icon, to }: { label: string; icon: React.ElementType; to: string }) {
  return (
    <Link
      to={to}
      className="flex items-center gap-2 rounded-lg border border-border bg-elevated px-4 py-3 text-sm text-muted transition hover:border-primary/40 hover:bg-primary-subtle hover:text-primary"
    >
      <Icon className="h-4 w-4" />
      <span>{label}</span>
    </Link>
  )
}

export function MissionControl() {
  const { data: fleet, isLoading: fleetLoading } = useFleetStatus()
  const { data: workItems = [], isLoading: workLoading } = useWorkItems()
  const { data: chats = [] } = useAgentSessions((data) => data.slice(0, 5))
  const { data: activity = [] } = useAuditRecent(10)

  const nodes = fleet?.nodes ?? []
  const summary = fleet?.summary ?? {}
  const onlineNodes = nodes.filter((n) => n.status === 'online' || n.status === 'connected')
  const healthPct = nodes.length > 0 ? Math.round((onlineNodes.length / nodes.length) * 100) : 0

  const activeTasks = workItems.filter((t) => t.status === 'building')
  const pendingTasks = workItems.filter((t) => t.status === 'ready' || t.status === 'blocked')
  const modelCount =
    summary.model_count ?? nodes.reduce((sum, n) => sum + (n.models?.length ?? 0), 0)

  const loading = fleetLoading || workLoading

  return (
    <section className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-foreground">Mission Control</h1>
          <p className="text-sm text-dim">ForgeFleet command center</p>
        </div>
      </div>

      {loading ? (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          {[1, 2, 3, 4].map((i) => (
            <div key={i} className="h-28 animate-pulse rounded-xl border border-border bg-panel" />
          ))}
        </div>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <StatCard
            label="Fleet Health"
            value={`${healthPct}%`}
            sub={`${onlineNodes.length}/${nodes.length} nodes online`}
            tone={healthPct === 100 ? 'ok' : healthPct >= 50 ? 'warn' : 'crit'}
            icon={Rocket}
          />
          <StatCard
            label="Active Tasks"
            value={activeTasks.length}
            sub={`${pendingTasks.length} pending`}
            tone="info"
            icon={Activity}
          />
          <StatCard
            label="Models Loaded"
            value={modelCount}
            sub={`across ${nodes.length} nodes`}
            icon={LayoutGrid}
          />
          <StatCard
            label="Recent Chats"
            value={chats.length}
            sub="sessions"
            tone="info"
            icon={MessageSquare}
          />
        </div>
      )}

      <div className="grid gap-6 lg:grid-cols-3">
        <div className="space-y-6 lg:col-span-2">
          <Card>
            <CardHeader>
              <CardTitle>Fleet Nodes</CardTitle>
              <Link to="/fleet" className="text-xs text-primary hover:text-primary-muted">
                View all
              </Link>
            </CardHeader>
            {nodes.length === 0 && !loading ? (
              <p className="text-sm text-dim">No fleet data available</p>
            ) : (
              <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-3">
                {nodes.slice(0, 6).map((node) => (
                  <Link
                    key={node.name}
                    to={`/nodes/${node.name}`}
                    className="flex items-center gap-3 rounded-lg border border-border bg-surface p-3 transition hover:border-border-subtle"
                  >
                    <span
                      className={`h-2 w-2 flex-shrink-0 rounded-full ${
                        node.status === 'online' || node.status === 'connected'
                          ? 'bg-status-ok'
                          : 'bg-dim'
                      }`}
                    />
                    <div className="min-w-0">
                      <p className="truncate text-sm font-medium capitalize text-foreground">
                        {node.name}
                      </p>
                      <p className="truncate text-xs text-dim">{node.hostname ?? node.ip ?? '—'}</p>
                    </div>
                  </Link>
                ))}
              </div>
            )}
          </Card>

          <Card>
            <CardHeader>
              <CardTitle>Live Work Board</CardTitle>
              <Link to="/my-tasks" className="text-xs text-primary hover:text-primary-muted">
                View all
              </Link>
            </CardHeader>
            {workItems.length === 0 ? (
              <p className="text-sm text-dim">No work items yet</p>
            ) : (
              <div className="space-y-2">
                {workItems.slice(0, 8).map((task) => (
                  <div
                    key={task.id}
                    className="flex items-center gap-3 rounded-lg border border-border bg-surface p-3"
                  >
                    <StatusBadge status={task.status} />
                    <p className="flex-1 truncate text-sm text-foreground">{task.title}</p>
                    {task.assignee && <span className="text-xs text-dim">{task.assignee}</span>}
                    {task.host && <span className="text-xs text-dim">@{task.host}</span>}
                  </div>
                ))}
              </div>
            )}
          </Card>
        </div>

        <div className="space-y-6">
          <WorkQueueMetrics />

          <Card>
            <CardHeader>
              <CardTitle>Quick Actions</CardTitle>
            </CardHeader>
            <div className="space-y-2">
              <QuickAction icon={MessageSquare} label="New Chat" to="/brain" />
              <QuickAction icon={Rocket} label="Fleet Overview" to="/fleet" />
              <QuickAction icon={LayoutGrid} label="Projects" to="/projects" />
              <QuickAction icon={Activity} label="Workflows" to="/workflow" />
            </div>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle>Recent Chats</CardTitle>
              <Link to="/brain" className="text-xs text-primary hover:text-primary-muted">
                View all
              </Link>
            </CardHeader>
            {chats.length === 0 ? (
              <p className="text-sm text-dim">No chats yet</p>
            ) : (
              <div className="space-y-2">
                {chats.map((chat) => (
                  <Link
                    key={chat.id}
                    to={`/brain/${chat.id}`}
                    className="block rounded-lg border border-border bg-surface p-3 transition hover:border-border-subtle"
                  >
                    <p className="truncate text-sm text-foreground">{chat.title || 'Untitled'}</p>
                    <div className="mt-1 flex items-center gap-2 text-xs text-dim">
                      {chat.model && <span>{chat.model}</span>}
                      {chat.created_at && <span>{timeAgo(chat.created_at)}</span>}
                    </div>
                  </Link>
                ))}
              </div>
            )}
          </Card>

          <Card>
            <CardHeader>
              <CardTitle>Activity</CardTitle>
              <Link to="/audit" className="text-xs text-primary hover:text-primary-muted">
                View all
              </Link>
            </CardHeader>
            {activity.length === 0 ? (
              <p className="text-sm text-dim">No recent activity</p>
            ) : (
              <div className="space-y-2">
                {activity.map((evt, i) => (
                  <div key={evt.id ?? i} className="rounded-lg border border-border bg-surface p-2">
                    <p className="truncate text-xs text-muted">
                      {evt.event_type.replace(/_/g, ' ')}
                    </p>
                    <div className="mt-0.5 flex items-center gap-2 text-[10px] text-dim">
                      {evt.actor && <span>{evt.actor}</span>}
                      <span>{timeAgo(evt.timestamp ?? evt.created_at)}</span>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </Card>
        </div>
      </div>
    </section>
  )
}
