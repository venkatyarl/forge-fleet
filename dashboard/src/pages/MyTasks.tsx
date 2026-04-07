import { type FormEvent, useCallback, useEffect, useMemo, useState } from 'react'
import { getJson, patchJson, postJson } from '../lib/api'

type WorkItemStatus = 'backlog' | 'todo' | 'in_progress' | 'review' | 'done' | 'blocked'

type McWorkItem = {
  id: string
  title: string
  description: string
  status: WorkItemStatus
  priority: number | { value?: number }
  assignee: string
  labels: string[]
  updated_at: string
}

type ActionKind = 'claim' | 'start' | 'review' | 'complete' | 'fail' | 'escalate'

const STATUS_ORDER: WorkItemStatus[] = ['backlog', 'todo', 'in_progress', 'review', 'blocked', 'done']

const STATUS_LABEL: Record<WorkItemStatus, string> = {
  backlog: 'Backlog',
  todo: 'Todo',
  in_progress: 'In Progress',
  review: 'Review',
  blocked: 'Blocked',
  done: 'Done',
}

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

function statusBadge(status: WorkItemStatus): string {
  switch (status) {
    case 'done':
      return 'bg-emerald-500/15 text-emerald-300 border-emerald-500/30'
    case 'blocked':
      return 'bg-rose-500/15 text-rose-300 border-rose-500/30'
    case 'review':
      return 'bg-purple-500/15 text-purple-300 border-purple-500/30'
    case 'in_progress':
      return 'bg-sky-500/15 text-sky-300 border-sky-500/30'
    case 'todo':
      return 'bg-amber-500/15 text-amber-300 border-amber-500/30'
    default:
      return 'bg-slate-500/15 text-slate-300 border-slate-500/30'
  }
}

function actionButtonStyle(kind: ActionKind): string {
  switch (kind) {
    case 'complete':
      return 'border-emerald-500/40 bg-emerald-500/10 text-emerald-300 hover:bg-emerald-500/20'
    case 'fail':
      return 'border-rose-500/40 bg-rose-500/10 text-rose-300 hover:bg-rose-500/20'
    case 'escalate':
      return 'border-orange-500/40 bg-orange-500/10 text-orange-300 hover:bg-orange-500/20'
    case 'review':
      return 'border-purple-500/40 bg-purple-500/10 text-purple-300 hover:bg-purple-500/20'
    case 'start':
      return 'border-sky-500/40 bg-sky-500/10 text-sky-300 hover:bg-sky-500/20'
    case 'claim':
    default:
      return 'border-slate-500/40 bg-slate-500/10 text-slate-300 hover:bg-slate-500/20'
  }
}

export function MyTasks() {
  const [items, setItems] = useState<McWorkItem[]>([])
  const [loading, setLoading] = useState(true)
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

  const load = useCallback(async () => {
    try {
      setError(null)
      const query = assigneeFilter.trim()
        ? `?assignee=${encodeURIComponent(assigneeFilter.trim())}`
        : ''
      const payload = await getJson<unknown>(`/api/mc/work-items${query}`)
      setItems(Array.isArray(payload) ? (payload as McWorkItem[]) : [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load work items')
    } finally {
      setLoading(false)
    }
  }, [assigneeFilter])

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
      const status = STATUS_ORDER.includes(item.status) ? item.status : 'backlog'
      byStatus[status].push(item)
    }

    for (const status of STATUS_ORDER) {
      byStatus[status].sort((a, b) => {
        const pDiff = priorityValue(a.priority) - priorityValue(b.priority)
        if (pDiff !== 0) return pDiff
        return new Date(b.updated_at).getTime() - new Date(a.updated_at).getTime()
      })
    }

    return byStatus
  }, [items])

  const openCount = items.filter((item) => item.status !== 'done').length
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
    <section className="space-y-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">My Tasks</h1>
          <p className="mt-1 text-sm text-slate-400">
            Personal queue parity screen for Mission Control work-item workflow.
          </p>
        </div>
        <button
          onClick={() => void load()}
          className="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500"
        >
          ↻ Refresh
        </button>
      </div>

      <div className="grid gap-3 sm:grid-cols-3">
        <StatCard label="Open" value={openCount} color="text-sky-300" />
        <StatCard label="In Flight" value={inFlightCount} color="text-purple-300" />
        <StatCard label="Blocked" value={blockedCount} color="text-rose-300" />
      </div>

      {error ? (
        <div className="rounded-xl border border-rose-500/30 bg-rose-500/10 px-4 py-3 text-sm text-rose-200">
          {error}
        </div>
      ) : null}
      {notice ? (
        <div className="rounded-xl border border-emerald-500/30 bg-emerald-500/10 px-4 py-3 text-sm text-emerald-200">
          {notice}
        </div>
      ) : null}

      <div className="rounded-xl border border-slate-800 bg-slate-900/50 p-4">
        <div className="mb-3 flex flex-wrap items-end gap-2">
          <label className="flex flex-col gap-1 text-xs text-slate-400">
            Assignee filter
            <input
              value={assigneeInput}
              onChange={(event) => setAssigneeInput(event.target.value)}
              placeholder="venkat / taylor / unassigned"
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
            />
          </label>
          <button
            onClick={() => setAssigneeFilter(assigneeInput.trim())}
            className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
          >
            Apply filter
          </button>
          <button
            onClick={() => {
              setAssigneeInput('')
              setAssigneeFilter('')
            }}
            className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-400 hover:bg-slate-800 hover:text-slate-200"
          >
            Clear
          </button>
          {assigneeFilter ? (
            <span className="rounded-full bg-slate-800 px-3 py-1 text-xs text-slate-300">
              filtering: {assigneeFilter}
            </span>
          ) : null}
        </div>

        <form onSubmit={createTask} className="grid gap-2 md:grid-cols-12">
          <input
            value={newTitle}
            onChange={(event) => setNewTitle(event.target.value)}
            placeholder="New task title"
            className="md:col-span-4 rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
            required
          />
          <input
            value={newDescription}
            onChange={(event) => setNewDescription(event.target.value)}
            placeholder="Description (optional)"
            className="md:col-span-4 rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
          />
          <input
            value={newAssignee}
            onChange={(event) => setNewAssignee(event.target.value)}
            placeholder="Assignee"
            className="md:col-span-2 rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
          />
          <select
            value={newStatus}
            onChange={(event) => setNewStatus(event.target.value as WorkItemStatus)}
            className="md:col-span-1 rounded-md border border-slate-700 bg-slate-950 px-2 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
          >
            {STATUS_ORDER.map((status) => (
              <option key={status} value={status}>
                {STATUS_LABEL[status]}
              </option>
            ))}
          </select>
          <select
            value={newPriority}
            onChange={(event) => setNewPriority(Number(event.target.value))}
            className="md:col-span-1 rounded-md border border-slate-700 bg-slate-950 px-2 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
          >
            <option value={1}>P1</option>
            <option value={2}>P2</option>
            <option value={3}>P3</option>
            <option value={4}>P4</option>
            <option value={5}>P5</option>
          </select>
          <input
            value={newLabels}
            onChange={(event) => setNewLabels(event.target.value)}
            placeholder="labels: backend,urgent"
            className="md:col-span-10 rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
          />
          <button
            type="submit"
            disabled={creating}
            className="md:col-span-2 rounded-md border border-sky-500/50 bg-sky-500/15 px-3 py-2 text-sm font-medium text-sky-300 hover:bg-sky-500/25 disabled:opacity-60"
          >
            {creating ? 'Creating…' : 'Create Task'}
          </button>
        </form>
      </div>

      {loading && items.length === 0 ? (
        <div className="rounded-xl border border-slate-800 bg-slate-900/50 px-4 py-3 text-sm text-slate-300">
          Loading work items…
        </div>
      ) : null}

      <div className="grid gap-4 xl:grid-cols-3">
        {STATUS_ORDER.map((status) => (
          <article key={status} className="rounded-xl border border-slate-800 bg-slate-900/60 p-3">
            <h2 className="mb-2 text-sm font-semibold uppercase tracking-wide text-slate-300">
              {STATUS_LABEL[status]} ({grouped[status].length})
            </h2>
            <div className="space-y-2">
              {grouped[status].length === 0 ? (
                <div className="rounded-md border border-slate-800 bg-slate-950/60 px-3 py-2 text-xs text-slate-500">
                  No tasks
                </div>
              ) : (
                grouped[status].map((item) => {
                  const priority = priorityValue(item.priority)
                  const busy = busyItemId === item.id

                  return (
                    <div key={item.id} className="rounded-lg border border-slate-800 bg-slate-950/70 p-3">
                      <div className="flex items-start justify-between gap-2">
                        <p className="font-medium text-slate-100">{item.title}</p>
                        <span className="rounded-md border border-slate-700 bg-slate-800 px-2 py-0.5 text-xs text-slate-300">
                          {priorityLabel(priority)}
                        </span>
                      </div>
                      {item.description ? (
                        <p className="mt-1 text-xs text-slate-400">{item.description}</p>
                      ) : null}

                      <div className="mt-2 flex flex-wrap items-center gap-1.5 text-xs text-slate-400">
                        <span className={`rounded-md border px-2 py-0.5 ${statusBadge(item.status)}`}>
                          {STATUS_LABEL[item.status]}
                        </span>
                        <span>assignee: {item.assignee || 'unassigned'}</span>
                        <span>
                          updated:{' '}
                          {new Date(item.updated_at).toLocaleString(undefined, {
                            month: 'short',
                            day: 'numeric',
                            hour: '2-digit',
                            minute: '2-digit',
                          })}
                        </span>
                      </div>

                      {item.labels.length > 0 ? (
                        <div className="mt-2 flex flex-wrap gap-1">
                          {item.labels.map((label) => (
                            <span
                              key={`${item.id}-${label}`}
                              className="rounded-full border border-slate-700 bg-slate-900 px-2 py-0.5 text-[11px] text-slate-300"
                            >
                              {label}
                            </span>
                          ))}
                        </div>
                      ) : null}

                      {item.status !== 'done' ? (
                        <div className="mt-2 flex flex-wrap gap-1.5">
                          {item.status === 'backlog' || item.status === 'todo' ? (
                            <ActionButton
                              label="Claim"
                              kind="claim"
                              disabled={busy}
                              onClick={() => void runAction(item, 'claim')}
                            />
                          ) : null}

                          {item.status !== 'in_progress' ? (
                            <ActionButton
                              label="Start"
                              kind="start"
                              disabled={busy}
                              onClick={() => void runAction(item, 'start')}
                            />
                          ) : null}

                          {item.status === 'in_progress' || item.status === 'review' ? (
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
          </article>
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
    <button
      onClick={onClick}
      disabled={disabled}
      className={`rounded-md border px-2 py-1 text-xs transition disabled:cursor-not-allowed disabled:opacity-60 ${actionButtonStyle(kind)}`}
      type="button"
    >
      {label}
    </button>
  )
}

function StatCard({ label, value, color }: { label: string; value: number; color: string }) {
  return (
    <div className="rounded-xl border border-slate-800 bg-slate-900/50 px-4 py-3">
      <dt className="text-xs uppercase tracking-wider text-slate-500">{label}</dt>
      <dd className={`text-2xl font-bold ${color}`}>{value}</dd>
    </div>
  )
}
