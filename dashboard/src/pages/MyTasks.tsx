import { type FormEvent, useCallback, useEffect, useMemo, useState } from 'react'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { Badge } from '../components/ui/badge'
import { StatusBadge } from '../components/ui/status-badge'
import { Button } from '../components/ui/button'
import { useWorkItems } from '../features/hooks/useDashboardQueries'
import { getJson, patchJson, postJson } from '../lib/api'
import { cn, formatElapsed } from '../lib/utils'

type WorkItemStatus = 'backlog' | 'todo' | 'in_progress' | 'review' | 'done' | 'blocked'

type McWorkItem = {
  id: string
  title: string
  description?: string
  status: WorkItemStatus
  priority?: number | { value?: number }
  assignee?: string
  labels?: string[]
  updated_at?: string
  updatedAt?: string
}

type ActionKind = 'claim' | 'start' | 'review' | 'complete' | 'fail' | 'escalate'
type StatTone = 'ok' | 'warn' | 'crit' | 'info'

const STATUS_ORDER: WorkItemStatus[] = ['backlog', 'todo', 'in_progress', 'review', 'blocked', 'done']

const STATUS_LABEL: Record<WorkItemStatus, string> = {
  backlog: 'Backlog',
  todo: 'Todo',
  in_progress: 'In Progress',
  review: 'Review',
  blocked: 'Blocked',
  done: 'Done',
}

const fieldClass =
  'rounded-lg border border-border bg-surface px-3 py-2 text-sm text-foreground outline-hidden transition placeholder:text-dim focus:border-primary'

function priorityValue(priority: McWorkItem['priority']): number {
  if (typeof priority === 'number') return priority
  if (priority && typeof priority === 'object' && typeof priority.value === 'number') {
    return priority.value
  }
  return 3
}

function priorityLabel(priority: number): string {
  switch (priority) {
    case 1:
      return 'critical'
    case 2:
      return 'high'
    case 3:
      return 'medium'
    case 4:
      return 'low'
    case 5:
      return 'minimal'
    default:
      return `p${priority}`
  }
}

function itemStatus(item: McWorkItem): WorkItemStatus {
  return STATUS_ORDER.includes(item.status) ? item.status : 'backlog'
}

function updatedAt(item: McWorkItem): string | undefined {
  return item.updated_at ?? item.updatedAt
}

function updatedLabel(item: McWorkItem): string {
  const timestamp = updatedAt(item)
  if (!timestamp) return 'unknown'
  const parsed = new Date(timestamp).getTime()
  if (Number.isNaN(parsed)) return 'unknown'
  const seconds = Math.max(0, Math.floor((Date.now() - parsed) / 1000))
  if (seconds < 5) return 'just now'
  return `${formatElapsed(seconds)} ago`
}

function priorityVariant(priority: number): 'crit' | 'warn' | 'info' | 'neutral' {
  if (priority <= 1) return 'crit'
  if (priority === 2) return 'warn'
  if (priority === 3) return 'info'
  return 'neutral'
}

function actionClass(kind: ActionKind): string {
  switch (kind) {
    case 'complete':
      return 'border-status-ok text-status-ok hover:bg-elevated'
    case 'fail':
      return 'border-status-crit text-status-crit hover:bg-elevated'
    case 'escalate':
      return 'border-status-warn text-status-warn hover:bg-elevated'
    case 'review':
      return 'border-primary text-primary hover:bg-primary-subtle'
    case 'start':
      return 'border-status-info text-status-info hover:bg-elevated'
    case 'claim':
    default:
      return 'border-border-subtle text-muted hover:bg-elevated hover:text-foreground'
  }
}

function statToneClass(tone: StatTone): string {
  switch (tone) {
    case 'ok':
      return 'text-status-ok'
    case 'warn':
      return 'text-status-warn'
    case 'crit':
      return 'text-status-crit'
    case 'info':
      return 'text-status-info'
  }
}

export function MyTasks() {
  const {
    data: queriedItems = [],
    isLoading: queryLoading,
    refetch: refetchWorkItems,
  } = useWorkItems()

  const [filteredItems, setFilteredItems] = useState<McWorkItem[] | null>(null)
  const [filteredLoading, setFilteredLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [busyItemId, setBusyItemId] = useState<string | null>(null)

  const [assigneeInput, setAssigneeInput] = useState('')
  const [assigneeFilter, setAssigneeFilter] = useState('')

  const [newTitle, setNewTitle] = useState('')
  const [newDescription, setNewDescription] = useState('')
  const [newAssignee, setNewAssignee] = useState('')
  const [newStatus, setNewStatus] = useState<WorkItemStatus>('backlog')
  const [newPriority, setNewPriority] = useState(3)
  const [newLabels, setNewLabels] = useState('')
  const [creating, setCreating] = useState(false)

  const items = assigneeFilter
    ? filteredItems ?? []
    : (queriedItems as unknown as McWorkItem[])
  const loading = assigneeFilter ? filteredLoading || filteredItems === null : queryLoading

  const load = useCallback(async () => {
    const assignee = assigneeFilter.trim()
    setError(null)

    if (!assignee) {
      setFilteredItems(null)
      await refetchWorkItems()
      return
    }

    try {
      setFilteredLoading(true)
      const payload = await getJson<unknown>(
        `/api/mc/work-items?assignee=${encodeURIComponent(assignee)}`,
      )
      setFilteredItems(Array.isArray(payload) ? (payload as McWorkItem[]) : [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load work items')
    } finally {
      setFilteredLoading(false)
    }
  }, [assigneeFilter, refetchWorkItems])

  useEffect(() => {
    void load()
    const id = window.setInterval(() => void load(), 15000)
    return () => window.clearInterval(id)
  }, [load])

  const grouped = useMemo(() => {
    const byStatus: Record<WorkItemStatus, McWorkItem[]> = {
      backlog: [],
      todo: [],
      in_progress: [],
      review: [],
      blocked: [],
      done: [],
    }

    for (const item of items) {
      byStatus[itemStatus(item)].push(item)
    }

    for (const status of STATUS_ORDER) {
      byStatus[status].sort((a, b) => {
        const pDiff = priorityValue(a.priority) - priorityValue(b.priority)
        if (pDiff !== 0) return pDiff
        return new Date(updatedAt(b) ?? 0).getTime() - new Date(updatedAt(a) ?? 0).getTime()
      })
    }

    return byStatus
  }, [items])

  const openCount = items.filter((item) => itemStatus(item) !== 'done').length
  const blockedCount = grouped.blocked.length
  const inFlightCount = grouped.in_progress.length + grouped.review.length

  const runAction = useCallback(
    async (item: McWorkItem, kind: ActionKind) => {
      try {
        setBusyItemId(item.id)
        setError(null)
        setNotice(null)

        if (kind === 'claim') {
          await postJson(`/api/mc/work-items/${item.id}/claim`, {
            assignee: assigneeFilter.trim() || item.assignee || 'unassigned',
          })
        } else if (kind === 'start') {
          await patchJson(`/api/mc/work-items/${item.id}`, { status: 'in_progress' })
        } else if (kind === 'review') {
          await postJson(`/api/mc/work-items/${item.id}/review/submit`, {})
        } else if (kind === 'complete') {
          await postJson(`/api/mc/work-items/${item.id}/complete`, {})
        } else if (kind === 'fail') {
          await postJson(`/api/mc/work-items/${item.id}/fail`, {})
        } else if (kind === 'escalate') {
          await postJson(`/api/mc/work-items/${item.id}/escalate`, {})
        }

        setNotice(`Updated task "${item.title}"`)
        await load()
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Action failed')
      } finally {
        setBusyItemId(null)
      }
    },
    [assigneeFilter, load],
  )

  const createTask = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!newTitle.trim()) return

    try {
      setCreating(true)
      setError(null)
      setNotice(null)

      const labels = newLabels
        .split(',')
        .map((label) => label.trim())
        .filter(Boolean)

      await postJson('/api/mc/work-items', {
        title: newTitle.trim(),
        description: newDescription.trim(),
        status: newStatus,
        priority: newPriority,
        assignee: newAssignee.trim() || undefined,
        labels,
      })

      setNewTitle('')
      setNewDescription('')
      setNewAssignee(assigneeFilter)
      setNewStatus('backlog')
      setNewPriority(3)
      setNewLabels('')
      setNotice('Created new work item')
      await load()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create work item')
    } finally {
      setCreating(false)
    }
  }

  return (
    <section className="space-y-6 bg-background text-foreground">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight text-foreground">My Tasks</h1>
          <p className="mt-1 text-sm text-dim">
            Personal Mission Control queue with live work-item actions.
          </p>
        </div>
        <Button onClick={() => void load()} disabled={loading} variant="secondary">
          {loading ? 'Refreshing' : 'Refresh'}
        </Button>
      </div>

      <div className="grid gap-3 sm:grid-cols-3">
        <StatCard label="Open" value={openCount} tone="info" />
        <StatCard label="In Flight" value={inFlightCount} tone="warn" />
        <StatCard label="Blocked" value={blockedCount} tone="crit" />
      </div>

      {error ? (
        <div className="rounded-xl border border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">
          {error}
        </div>
      ) : null}
      {notice ? (
        <div className="rounded-xl border border-status-ok bg-panel px-4 py-3 text-sm text-status-ok">
          {notice}
        </div>
      ) : null}

      <Card>
        <CardHeader className="items-start gap-3">
          <div>
            <CardTitle>Queue Controls</CardTitle>
            <CardDescription>Filter work ownership and create new Mission Control items.</CardDescription>
          </div>
          {assigneeFilter ? <Badge variant="default">assignee: {assigneeFilter}</Badge> : null}
        </CardHeader>

        <div className="mb-4 flex flex-wrap items-end gap-2">
          <label className="flex min-w-64 flex-col gap-1 text-xs font-medium text-muted">
            Assignee filter
            <input
              value={assigneeInput}
              onChange={(event) => setAssigneeInput(event.target.value)}
              placeholder="venkat / taylor / unassigned"
              className={fieldClass}
            />
          </label>
          <Button
            onClick={() => setAssigneeFilter(assigneeInput.trim())}
            type="button"
            variant="outline"
          >
            Apply filter
          </Button>
          <Button
            onClick={() => {
              setAssigneeInput('')
              setAssigneeFilter('')
            }}
            type="button"
            variant="ghost"
          >
            Clear
          </Button>
        </div>

        <form onSubmit={createTask} className="grid gap-2 md:grid-cols-12">
          <input
            aria-label="New task title"
            value={newTitle}
            onChange={(event) => setNewTitle(event.target.value)}
            placeholder="New task title"
            className={cn(fieldClass, 'md:col-span-4')}
            required
          />
          <input
            aria-label="New task description"
            value={newDescription}
            onChange={(event) => setNewDescription(event.target.value)}
            placeholder="Description (optional)"
            className={cn(fieldClass, 'md:col-span-4')}
          />
          <input
            aria-label="New task assignee"
            value={newAssignee}
            onChange={(event) => setNewAssignee(event.target.value)}
            placeholder="Assignee"
            className={cn(fieldClass, 'md:col-span-2')}
          />
          <select
            aria-label="New task status"
            value={newStatus}
            onChange={(event) => setNewStatus(event.target.value as WorkItemStatus)}
            className={cn(fieldClass, 'md:col-span-1')}
          >
            {STATUS_ORDER.map((status) => (
              <option key={status} value={status}>
                {STATUS_LABEL[status]}
              </option>
            ))}
          </select>
          <select
            aria-label="New task priority"
            value={newPriority}
            onChange={(event) => setNewPriority(Number(event.target.value))}
            className={cn(fieldClass, 'md:col-span-1')}
          >
            <option value={1}>P1</option>
            <option value={2}>P2</option>
            <option value={3}>P3</option>
            <option value={4}>P4</option>
            <option value={5}>P5</option>
          </select>
          <input
            aria-label="New task labels"
            value={newLabels}
            onChange={(event) => setNewLabels(event.target.value)}
            placeholder="labels: backend,urgent"
            className={cn(fieldClass, 'md:col-span-10')}
          />
          <Button type="submit" disabled={creating} className="md:col-span-2">
            {creating ? 'Creating' : 'Create Task'}
          </Button>
        </form>
      </Card>

      {loading && items.length === 0 ? (
        <Card className="bg-surface">
          <p className="text-sm text-muted">Loading work items...</p>
        </Card>
      ) : null}

      <div className="grid gap-4 xl:grid-cols-3">
        {STATUS_ORDER.map((status) => (
          <Card key={status} className="bg-panel p-3">
            <CardHeader className="mb-3">
              <div>
                <CardTitle className="uppercase tracking-wide">{STATUS_LABEL[status]}</CardTitle>
                <CardDescription>{grouped[status].length} work items</CardDescription>
              </div>
              <Badge variant="neutral">{grouped[status].length}</Badge>
            </CardHeader>

            <div className="space-y-2">
              {grouped[status].length === 0 ? (
                <div className="rounded-lg border border-border bg-surface px-3 py-2 text-xs text-dim">
                  No tasks
                </div>
              ) : (
                grouped[status].map((item) => {
                  const statusValue = itemStatus(item)
                  const priority = priorityValue(item.priority)
                  const busy = busyItemId === item.id
                  const labels = item.labels ?? []

                  return (
                    <div
                      key={item.id}
                      className="rounded-lg border border-border bg-surface p-3 transition hover:border-border-subtle"
                    >
                      <div className="flex items-start justify-between gap-2">
                        <p className="min-w-0 wrap-break-word text-sm font-medium text-foreground">
                          {item.title}
                        </p>
                        <Badge variant={priorityVariant(priority)}>{priorityLabel(priority)}</Badge>
                      </div>
                      {item.description ? (
                        <p className="mt-1 wrap-break-word text-xs text-muted">{item.description}</p>
                      ) : null}

                      <div className="mt-3 flex flex-wrap items-center gap-1.5 text-xs text-dim">
                        <StatusBadge status={statusValue}>{STATUS_LABEL[statusValue]}</StatusBadge>
                        <span>assignee: {item.assignee || 'unassigned'}</span>
                        <span>updated: {updatedLabel(item)}</span>
                      </div>

                      {labels.length > 0 ? (
                        <div className="mt-2 flex flex-wrap gap-1">
                          {labels.map((label) => (
                            <Badge key={`${item.id}-${label}`} variant="neutral">
                              {label}
                            </Badge>
                          ))}
                        </div>
                      ) : null}

                      {statusValue !== 'done' ? (
                        <div className="mt-3 flex flex-wrap gap-1.5">
                          {statusValue === 'backlog' || statusValue === 'todo' ? (
                            <ActionButton
                              label="Claim"
                              kind="claim"
                              disabled={busy}
                              onClick={() => void runAction(item, 'claim')}
                            />
                          ) : null}

                          {statusValue !== 'in_progress' ? (
                            <ActionButton
                              label="Start"
                              kind="start"
                              disabled={busy}
                              onClick={() => void runAction(item, 'start')}
                            />
                          ) : null}

                          {statusValue === 'in_progress' || statusValue === 'review' ? (
                            <ActionButton
                              label="Review"
                              kind="review"
                              disabled={busy}
                              onClick={() => void runAction(item, 'review')}
                            />
                          ) : null}

                          <ActionButton
                            label="Complete"
                            kind="complete"
                            disabled={busy}
                            onClick={() => void runAction(item, 'complete')}
                          />
                          <ActionButton
                            label="Block"
                            kind="fail"
                            disabled={busy}
                            onClick={() => void runAction(item, 'fail')}
                          />
                          <ActionButton
                            label="Escalate"
                            kind="escalate"
                            disabled={busy}
                            onClick={() => void runAction(item, 'escalate')}
                          />
                        </div>
                      ) : null}
                    </div>
                  )
                })
              )}
            </div>
          </Card>
        ))}
      </div>
    </section>
  )
}

function ActionButton({
  label,
  kind,
  disabled,
  onClick,
}: {
  label: string
  kind: ActionKind
  disabled: boolean
  onClick: () => void
}) {
  return (
    <Button
      onClick={onClick}
      disabled={disabled}
      className={cn('bg-transparent', actionClass(kind))}
      type="button"
      size="sm"
      variant="outline"
    >
      {label}
    </Button>
  )
}

function StatCard({ label, value, tone }: { label: string; value: number; tone: StatTone }) {
  return (
    <Card>
      <CardHeader className="mb-2">
        <CardDescription>{label}</CardDescription>
      </CardHeader>
      <div className={cn('text-2xl font-bold', statToneClass(tone))}>{value}</div>
    </Card>
  )
}
