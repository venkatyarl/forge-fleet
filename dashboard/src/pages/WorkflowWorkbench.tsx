import { type FormEvent, useCallback, useEffect, useMemo, useState } from 'react'
import { deleteJson, getJson, patchJson, postJson } from '../lib/api'

type WorkItemStatus = 'backlog' | 'todo' | 'in_progress' | 'review' | 'done' | 'blocked'
type ReviewStatus = 'pending' | 'in_progress' | 'approved' | 'changes_requested'

type WorkItem = {
  id: string
  title: string
  description: string
  status: WorkItemStatus | string
  priority: number | { value?: number }
  assignee: string
  epic_id?: string | null
  sprint_id?: string | null
  task_group_id?: string | null
  sequence_order?: number | null
  labels: string[]
  updated_at: string
}

type ReviewItem = {
  id: string
  work_item_id: string
  title: string
  status: ReviewStatus | string
  reviewer?: string | null
  notes?: string | null
  updated_at: string
}

type TaskGroup = {
  id: string
  name: string
  description: string
}

type WorkItemDependency = {
  work_item_id: string
  depends_on_id: string
  created_at: string
}

type DependencyCheck = {
  work_item_id: string
  blocked_by_ids: string[]
  blocked_count: number
  can_start: boolean
}

type WorkflowAction =
  | 'claim'
  | 'start'
  | 'start_review'
  | 'submit_review'
  | 'complete_review'
  | 'complete'
  | 'fail'
  | 'escalate'

const STATUS_FILTERS: Array<{ value: ''; label: 'All statuses' } | { value: WorkItemStatus; label: string }> = [
  { value: '', label: 'All statuses' },
  { value: 'backlog', label: 'Backlog' },
  { value: 'todo', label: 'Todo' },
  { value: 'in_progress', label: 'In Progress' },
  { value: 'review', label: 'Review' },
  { value: 'blocked', label: 'Blocked' },
  { value: 'done', label: 'Done' },
]

const REVIEW_STATUSES: ReviewStatus[] = ['pending', 'in_progress', 'approved', 'changes_requested']

function asPriority(priority: WorkItem['priority']): number {
  if (typeof priority === 'number') return priority
  if (priority && typeof priority === 'object' && typeof priority.value === 'number') {
    return priority.value
  }
  return 3
}

function statusBadge(status: string): string {
  switch (status) {
    case 'done':
      return 'border-emerald-500/30 bg-emerald-500/10 text-emerald-300'
    case 'review':
      return 'border-purple-500/30 bg-purple-500/10 text-purple-300'
    case 'in_progress':
      return 'border-sky-500/30 bg-sky-500/10 text-sky-300'
    case 'blocked':
      return 'border-rose-500/30 bg-rose-500/10 text-rose-300'
    case 'todo':
      return 'border-amber-500/30 bg-amber-500/10 text-amber-300'
    default:
      return 'border-slate-700 bg-slate-800 text-slate-300'
  }
}

function reviewBadge(status: string): string {
  switch (status) {
    case 'approved':
      return 'border-emerald-500/30 bg-emerald-500/10 text-emerald-300'
    case 'changes_requested':
      return 'border-rose-500/30 bg-rose-500/10 text-rose-300'
    case 'in_progress':
      return 'border-sky-500/30 bg-sky-500/10 text-sky-300'
    default:
      return 'border-slate-700 bg-slate-800 text-slate-300'
  }
}

function labelize(value: string): string {
  return value
    .replaceAll('_', ' ')
    .split(' ')
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(' ')
}

function fmtDate(value: string): string {
  try {
    return new Date(value).toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    })
  } catch {
    return value
  }
}

export function WorkflowWorkbench() {
  const [items, setItems] = useState<WorkItem[]>([])
  const [groups, setGroups] = useState<TaskGroup[]>([])
  const [reviews, setReviews] = useState<ReviewItem[]>([])
  const [dependencies, setDependencies] = useState<WorkItemDependency[]>([])
  const [depCheck, setDepCheck] = useState<DependencyCheck | null>(null)

  const [selectedItemId, setSelectedItemId] = useState('')
  const [statusFilter, setStatusFilter] = useState<WorkItemStatus | ''>('')
  const [assigneeFilter, setAssigneeFilter] = useState('')

  const [newDependencyId, setNewDependencyId] = useState('')
  const [newReviewTitle, setNewReviewTitle] = useState('')
  const [newReviewer, setNewReviewer] = useState('')
  const [newReviewNotes, setNewReviewNotes] = useState('')

  const [newGroupName, setNewGroupName] = useState('')
  const [newGroupDescription, setNewGroupDescription] = useState('')
  const [groupSelection, setGroupSelection] = useState('')
  const [sequenceOrder, setSequenceOrder] = useState('')

  const [loading, setLoading] = useState(true)
  const [loadingDetails, setLoadingDetails] = useState(false)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)

  const selectedItem = useMemo(
    () => items.find((item) => item.id === selectedItemId) ?? null,
    [items, selectedItemId],
  )

  const itemById = useMemo(() => {
    const map = new Map<string, WorkItem>()
    for (const item of items) map.set(item.id, item)
    return map
  }, [items])

  const loadCore = useCallback(async () => {
    try {
      setError(null)

      const params = new URLSearchParams()
      if (statusFilter) params.set('status', statusFilter)
      if (assigneeFilter.trim()) params.set('assignee', assigneeFilter.trim())

      const query = params.toString()
      const [workItemsPayload, groupsPayload] = await Promise.all([
        getJson<unknown>(`/api/mc/work-items${query ? `?${query}` : ''}`),
        getJson<unknown>('/api/mc/task-groups'),
      ])

      const nextItems = Array.isArray(workItemsPayload) ? (workItemsPayload as WorkItem[]) : []
      const nextGroups = Array.isArray(groupsPayload) ? (groupsPayload as TaskGroup[]) : []

      setItems(nextItems)
      setGroups(nextGroups)

      if (nextItems.length === 0) {
        setSelectedItemId('')
      } else if (!nextItems.some((item) => item.id === selectedItemId)) {
        setSelectedItemId(nextItems[0].id)
      }

      if (nextGroups.length > 0 && !groupSelection) {
        setGroupSelection(nextGroups[0].id)
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load workflow data')
    } finally {
      setLoading(false)
    }
  }, [assigneeFilter, groupSelection, selectedItemId, statusFilter])

  const loadDetails = useCallback(async (workItemId: string) => {
    if (!workItemId) {
      setReviews([])
      setDependencies([])
      setDepCheck(null)
      return
    }

    try {
      setLoadingDetails(true)
      setError(null)

      const [reviewsPayload, dependenciesPayload, checkPayload] = await Promise.all([
        getJson<unknown>(`/api/mc/work-items/${workItemId}/review-items`),
        getJson<unknown>(`/api/mc/work-items/${workItemId}/dependencies`),
        getJson<DependencyCheck>(`/api/mc/work-items/${workItemId}/dependencies/check`),
      ])

      setReviews(Array.isArray(reviewsPayload) ? (reviewsPayload as ReviewItem[]) : [])
      setDependencies(Array.isArray(dependenciesPayload) ? (dependenciesPayload as WorkItemDependency[]) : [])
      setDepCheck(checkPayload)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load selected item details')
      setReviews([])
      setDependencies([])
      setDepCheck(null)
    } finally {
      setLoadingDetails(false)
    }
  }, [])

  useEffect(() => {
    void loadCore()
  }, [loadCore])

  useEffect(() => {
    if (!selectedItemId) {
      setReviews([])
      setDependencies([])
      setDepCheck(null)
      return
    }
    void loadDetails(selectedItemId)
  }, [loadDetails, selectedItemId])

  const refreshAll = async () => {
    await loadCore()
    if (selectedItemId) {
      await loadDetails(selectedItemId)
    }
  }

  const runAction = useCallback(
    async (action: WorkflowAction) => {
      if (!selectedItem) return

      try {
        setSaving(true)
        setError(null)
        setNotice(null)

        if (action === 'claim') {
          await postJson(`/api/mc/work-items/${selectedItem.id}/claim`, {
            assignee: assigneeFilter.trim() || selectedItem.assignee || 'unassigned',
          })
        } else if (action === 'start') {
          await patchJson(`/api/mc/work-items/${selectedItem.id}`, { status: 'in_progress' })
        } else if (action === 'start_review') {
          await postJson(`/api/mc/work-items/${selectedItem.id}/review/start`, {})
        } else if (action === 'submit_review') {
          await postJson(`/api/mc/work-items/${selectedItem.id}/review/submit`, {})
        } else if (action === 'complete_review') {
          await postJson(`/api/mc/work-items/${selectedItem.id}/review/complete`, {})
        } else if (action === 'complete') {
          await postJson(`/api/mc/work-items/${selectedItem.id}/complete`, {})
        } else if (action === 'fail') {
          await postJson(`/api/mc/work-items/${selectedItem.id}/fail`, {})
        } else if (action === 'escalate') {
          await postJson(`/api/mc/work-items/${selectedItem.id}/escalate`, {})
        }

        setNotice(`Action applied to "${selectedItem.title}"`)
        await loadCore()
        await loadDetails(selectedItem.id)
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Workflow action failed')
      } finally {
        setSaving(false)
      }
    },
    [assigneeFilter, loadCore, loadDetails, selectedItem],
  )

  const addDependency = async () => {
    if (!selectedItemId || !newDependencyId) return

    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await postJson(`/api/mc/work-items/${selectedItemId}/dependencies`, {
        depends_on_id: newDependencyId,
      })

      setNewDependencyId('')
      setNotice('Dependency added')
      await loadDetails(selectedItemId)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to add dependency')
    } finally {
      setSaving(false)
    }
  }

  const removeDependency = async (dependsOnId: string) => {
    if (!selectedItemId) return

    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await deleteJson(`/api/mc/work-items/${selectedItemId}/dependencies/${dependsOnId}`)

      setNotice('Dependency removed')
      await loadDetails(selectedItemId)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to remove dependency')
    } finally {
      setSaving(false)
    }
  }

  const createReviewItem = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!selectedItemId || !newReviewTitle.trim()) return

    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await postJson(`/api/mc/work-items/${selectedItemId}/review-items`, {
        title: newReviewTitle.trim(),
        reviewer: newReviewer.trim() || undefined,
        notes: newReviewNotes.trim() || undefined,
      })

      setNewReviewTitle('')
      setNewReviewer('')
      setNewReviewNotes('')
      setNotice('Review checklist item created')
      await loadDetails(selectedItemId)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create review item')
    } finally {
      setSaving(false)
    }
  }

  const updateReviewStatus = async (reviewId: string, status: ReviewStatus) => {
    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await patchJson(`/api/mc/review-items/${reviewId}`, { status })
      setNotice('Review status updated')
      if (selectedItemId) {
        await loadDetails(selectedItemId)
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update review status')
    } finally {
      setSaving(false)
    }
  }

  const removeReviewItem = async (reviewId: string) => {
    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await deleteJson(`/api/mc/review-items/${reviewId}`)
      setNotice('Review item deleted')
      if (selectedItemId) {
        await loadDetails(selectedItemId)
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to delete review item')
    } finally {
      setSaving(false)
    }
  }

  const resetReviewChecklist = async () => {
    if (!selectedItemId) return

    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await postJson(`/api/mc/work-items/${selectedItemId}/review-items/reset`, {})
      setNotice('Review checklist reset')
      await loadDetails(selectedItemId)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to reset review checklist')
    } finally {
      setSaving(false)
    }
  }

  const createTaskGroup = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!newGroupName.trim()) return

    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await postJson('/api/mc/task-groups', {
        name: newGroupName.trim(),
        description: newGroupDescription.trim(),
      })

      setNewGroupName('')
      setNewGroupDescription('')
      setNotice('Task group created')
      await loadCore()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create task group')
    } finally {
      setSaving(false)
    }
  }

  const assignToTaskGroup = async () => {
    if (!selectedItemId || !groupSelection) return

    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      const parsed = Number.parseInt(sequenceOrder, 10)
      const payload = Number.isNaN(parsed) ? {} : { sequence_order: parsed }

      await postJson(`/api/mc/task-groups/${groupSelection}/items/${selectedItemId}`, payload)
      setNotice('Work item assigned to task group')
      await loadCore()
      await loadDetails(selectedItemId)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to assign task group')
    } finally {
      setSaving(false)
    }
  }

  const unassignTaskGroup = async () => {
    if (!selectedItem || !selectedItem.task_group_id) return

    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await deleteJson(`/api/mc/task-groups/${selectedItem.task_group_id}/items/${selectedItem.id}`)

      setNotice('Work item removed from task group')
      await loadCore()
      await loadDetails(selectedItem.id)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to unassign task group')
    } finally {
      setSaving(false)
    }
  }

  const openCount = items.filter((item) => item.status !== 'done').length
  const blockedCount = items.filter((item) => item.status === 'blocked').length
  const reviewCount = items.filter((item) => item.status === 'review').length

  return (
    <section className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Workflow Workbench</h1>
          <p className="mt-1 text-sm text-slate-400">
            Mission Control workflow parity for review lifecycle, dependencies, and task-group sequencing.
          </p>
        </div>
        <button
          onClick={() => void refreshAll()}
          disabled={loading || saving}
          className="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-60"
        >
          ↻ Refresh
        </button>
      </div>

      <div className="grid gap-3 sm:grid-cols-3">
        <Stat label="Open items" value={openCount} color="text-sky-300" />
        <Stat label="In review" value={reviewCount} color="text-purple-300" />
        <Stat label="Blocked" value={blockedCount} color="text-rose-300" />
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
        <div className="grid gap-2 md:grid-cols-4">
          <select
            value={statusFilter}
            onChange={(event) => setStatusFilter(event.target.value as WorkItemStatus | '')}
            className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
          >
            {STATUS_FILTERS.map((opt) => (
              <option key={opt.label} value={opt.value}>
                {opt.label}
              </option>
            ))}
          </select>
          <input
            value={assigneeFilter}
            onChange={(event) => setAssigneeFilter(event.target.value)}
            placeholder="Assignee filter"
            className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
          />
          <button
            onClick={() => void loadCore()}
            className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
          >
            Apply filters
          </button>
          <button
            onClick={() => {
              setStatusFilter('')
              setAssigneeFilter('')
            }}
            className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-400 hover:bg-slate-800 hover:text-slate-200"
          >
            Clear filters
          </button>
        </div>
      </div>

      <div className="grid gap-4 xl:grid-cols-12">
        <div className="xl:col-span-5">
          <div className="rounded-xl border border-slate-800 bg-slate-900/60 p-3">
            <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">
              Work items ({items.length})
            </h2>
            {loading ? (
              <p className="text-sm text-slate-400">Loading work items…</p>
            ) : items.length === 0 ? (
              <p className="text-sm text-slate-500">No work items match this filter.</p>
            ) : (
              <div className="space-y-2">
                {items.map((item) => (
                  <button
                    key={item.id}
                    type="button"
                    onClick={() => setSelectedItemId(item.id)}
                    className={`w-full rounded-lg border p-3 text-left transition ${
                      item.id === selectedItemId
                        ? 'border-sky-500/50 bg-sky-500/10'
                        : 'border-slate-800 bg-slate-950/60 hover:border-slate-600'
                    }`}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <p className="font-medium text-slate-100">{item.title}</p>
                      <span className="rounded-md border border-slate-700 bg-slate-800 px-2 py-0.5 text-xs text-slate-300">
                        P{asPriority(item.priority)}
                      </span>
                    </div>
                    <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-slate-400">
                      <span className={`rounded border px-2 py-0.5 ${statusBadge(String(item.status))}`}>
                        {labelize(String(item.status))}
                      </span>
                      <span>{item.assignee || 'unassigned'}</span>
                      <span>updated {fmtDate(item.updated_at)}</span>
                    </div>
                  </button>
                ))}
              </div>
            )}
          </div>
        </div>

        <div className="xl:col-span-7 space-y-4">
          {!selectedItem ? (
            <div className="rounded-xl border border-slate-800 bg-slate-900/60 px-4 py-5 text-sm text-slate-400">
              Select a work item to manage workflow actions.
            </div>
          ) : (
            <>
              <section className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div>
                    <h2 className="text-lg font-semibold text-slate-100">{selectedItem.title}</h2>
                    <p className="mt-1 text-xs text-slate-400">ID: {selectedItem.id}</p>
                    {selectedItem.description ? (
                      <p className="mt-2 text-sm text-slate-300">{selectedItem.description}</p>
                    ) : null}
                  </div>
                  <span className={`rounded-md border px-2 py-1 text-xs ${statusBadge(String(selectedItem.status))}`}>
                    {labelize(String(selectedItem.status))}
                  </span>
                </div>

                <div className="mt-3 flex flex-wrap gap-2">
                  <ActionButton label="Claim" disabled={saving} onClick={() => void runAction('claim')} />
                  <ActionButton label="Start" disabled={saving} onClick={() => void runAction('start')} />
                  <ActionButton
                    label="Review Start"
                    disabled={saving}
                    onClick={() => void runAction('start_review')}
                  />
                  <ActionButton
                    label="Review Submit"
                    disabled={saving}
                    onClick={() => void runAction('submit_review')}
                  />
                  <ActionButton
                    label="Review Complete"
                    disabled={saving}
                    onClick={() => void runAction('complete_review')}
                  />
                  <ActionButton
                    label="Complete"
                    tone="ok"
                    disabled={saving}
                    onClick={() => void runAction('complete')}
                  />
                  <ActionButton
                    label="Block"
                    tone="danger"
                    disabled={saving}
                    onClick={() => void runAction('fail')}
                  />
                  <ActionButton
                    label="Escalate"
                    tone="warn"
                    disabled={saving}
                    onClick={() => void runAction('escalate')}
                  />
                </div>
              </section>

              <section className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
                <div className="mb-3 flex items-center justify-between">
                  <h3 className="text-sm font-semibold uppercase tracking-wide text-slate-300">Dependencies</h3>
                  <button
                    onClick={() => selectedItemId && void loadDetails(selectedItemId)}
                    className="rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-300 hover:bg-slate-800"
                  >
                    Recheck
                  </button>
                </div>

                {loadingDetails ? (
                  <p className="text-xs text-slate-400">Loading dependency state…</p>
                ) : depCheck ? (
                  <p className="text-xs text-slate-400">
                    {depCheck.can_start
                      ? 'Ready to start: no open blockers.'
                      : `${depCheck.blocked_count} blocker(s) still open.`}
                  </p>
                ) : null}

                <div className="mt-3 grid gap-2 md:grid-cols-[1fr_auto]">
                  <select
                    value={newDependencyId}
                    onChange={(event) => setNewDependencyId(event.target.value)}
                    className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
                  >
                    <option value="">Select dependency work item…</option>
                    {items
                      .filter((item) => item.id !== selectedItem.id)
                      .map((item) => (
                        <option key={item.id} value={item.id}>
                          {item.title}
                        </option>
                      ))}
                  </select>
                  <button
                    onClick={() => void addDependency()}
                    disabled={saving || !newDependencyId}
                    className="rounded-md border border-sky-500/40 bg-sky-500/15 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/25 disabled:opacity-60"
                  >
                    Add dependency
                  </button>
                </div>

                <div className="mt-3 space-y-2">
                  {dependencies.length === 0 ? (
                    <p className="text-xs text-slate-500">No dependencies configured.</p>
                  ) : (
                    dependencies.map((dep) => {
                      const target = itemById.get(dep.depends_on_id)
                      const blocked = depCheck?.blocked_by_ids.includes(dep.depends_on_id) ?? false
                      return (
                        <div
                          key={`${dep.work_item_id}-${dep.depends_on_id}`}
                          className="flex items-center justify-between rounded-md border border-slate-800 bg-slate-950/70 px-3 py-2"
                        >
                          <div>
                            <p className="text-sm text-slate-200">{target?.title ?? dep.depends_on_id}</p>
                            <p className="text-xs text-slate-500">
                              {blocked ? 'Blocking' : 'Resolved'} · {fmtDate(dep.created_at)}
                            </p>
                          </div>
                          <button
                            onClick={() => void removeDependency(dep.depends_on_id)}
                            className="rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-400 hover:bg-slate-800 hover:text-slate-200"
                          >
                            Remove
                          </button>
                        </div>
                      )
                    })
                  )}
                </div>
              </section>

              <section className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
                <div className="mb-3 flex items-center justify-between">
                  <h3 className="text-sm font-semibold uppercase tracking-wide text-slate-300">Review checklist</h3>
                  <button
                    onClick={() => void resetReviewChecklist()}
                    disabled={saving || reviews.length === 0}
                    className="rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-300 hover:bg-slate-800 disabled:opacity-60"
                  >
                    Reset all
                  </button>
                </div>

                <form onSubmit={createReviewItem} className="grid gap-2 md:grid-cols-6">
                  <input
                    value={newReviewTitle}
                    onChange={(event) => setNewReviewTitle(event.target.value)}
                    placeholder="Checklist title"
                    className="md:col-span-2 rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
                    required
                  />
                  <input
                    value={newReviewer}
                    onChange={(event) => setNewReviewer(event.target.value)}
                    placeholder="Reviewer"
                    className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
                  />
                  <input
                    value={newReviewNotes}
                    onChange={(event) => setNewReviewNotes(event.target.value)}
                    placeholder="Notes"
                    className="md:col-span-2 rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
                  />
                  <button
                    type="submit"
                    disabled={saving}
                    className="rounded-md border border-sky-500/40 bg-sky-500/15 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/25 disabled:opacity-60"
                  >
                    Add
                  </button>
                </form>

                <div className="mt-3 space-y-2">
                  {reviews.length === 0 ? (
                    <p className="text-xs text-slate-500">No checklist items yet.</p>
                  ) : (
                    reviews.map((item) => (
                      <div key={item.id} className="rounded-md border border-slate-800 bg-slate-950/70 p-3">
                        <div className="flex flex-wrap items-center justify-between gap-2">
                          <p className="text-sm text-slate-200">{item.title}</p>
                          <div className="flex items-center gap-2">
                            <span className={`rounded border px-2 py-0.5 text-xs ${reviewBadge(String(item.status))}`}>
                              {labelize(String(item.status))}
                            </span>
                            <button
                              onClick={() => void removeReviewItem(item.id)}
                              className="rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-400 hover:bg-slate-800 hover:text-slate-200"
                            >
                              Delete
                            </button>
                          </div>
                        </div>
                        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-slate-400">
                          <span>Reviewer: {item.reviewer || 'unassigned'}</span>
                          <span>Updated: {fmtDate(item.updated_at)}</span>
                        </div>
                        {item.notes ? <p className="mt-1 text-xs text-slate-500">{item.notes}</p> : null}
                        <div className="mt-2 flex flex-wrap gap-1">
                          {REVIEW_STATUSES.map((status) => (
                            <button
                              key={`${item.id}-${status}`}
                              onClick={() => void updateReviewStatus(item.id, status)}
                              disabled={saving}
                              className={`rounded-md border px-2 py-1 text-xs transition ${
                                String(item.status) === status
                                  ? 'border-sky-500/40 bg-sky-500/10 text-sky-300'
                                  : 'border-slate-700 text-slate-300 hover:bg-slate-800'
                              }`}
                            >
                              {labelize(status)}
                            </button>
                          ))}
                        </div>
                      </div>
                    ))
                  )}
                </div>
              </section>

              <section className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
                <h3 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">Task groups</h3>

                <form onSubmit={createTaskGroup} className="grid gap-2 md:grid-cols-4">
                  <input
                    value={newGroupName}
                    onChange={(event) => setNewGroupName(event.target.value)}
                    placeholder="New task group"
                    className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
                    required
                  />
                  <input
                    value={newGroupDescription}
                    onChange={(event) => setNewGroupDescription(event.target.value)}
                    placeholder="Description"
                    className="md:col-span-2 rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
                  />
                  <button
                    type="submit"
                    disabled={saving}
                    className="rounded-md border border-sky-500/40 bg-sky-500/15 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/25 disabled:opacity-60"
                  >
                    Create
                  </button>
                </form>

                <div className="mt-3 grid gap-2 md:grid-cols-[1fr_120px_auto_auto]">
                  <select
                    value={groupSelection}
                    onChange={(event) => setGroupSelection(event.target.value)}
                    className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
                  >
                    <option value="">Select task group…</option>
                    {groups.map((group) => (
                      <option key={group.id} value={group.id}>
                        {group.name}
                      </option>
                    ))}
                  </select>
                  <input
                    value={sequenceOrder}
                    onChange={(event) => setSequenceOrder(event.target.value)}
                    inputMode="numeric"
                    placeholder="Seq"
                    className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
                  />
                  <button
                    onClick={() => void assignToTaskGroup()}
                    disabled={saving || !groupSelection}
                    className="rounded-md border border-sky-500/40 bg-sky-500/15 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/25 disabled:opacity-60"
                  >
                    Assign selected
                  </button>
                  <button
                    onClick={() => void unassignTaskGroup()}
                    disabled={saving || !selectedItem.task_group_id}
                    className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-300 hover:bg-slate-800 disabled:opacity-60"
                  >
                    Unassign
                  </button>
                </div>

                <div className="mt-3 space-y-2">
                  {groups.length === 0 ? (
                    <p className="text-xs text-slate-500">No task groups yet.</p>
                  ) : (
                    groups.map((group) => (
                      <div
                        key={group.id}
                        className={`rounded-md border px-3 py-2 ${
                          selectedItem.task_group_id === group.id
                            ? 'border-sky-500/40 bg-sky-500/10'
                            : 'border-slate-800 bg-slate-950/70'
                        }`}
                      >
                        <p className="text-sm text-slate-200">{group.name}</p>
                        {group.description ? (
                          <p className="text-xs text-slate-500">{group.description}</p>
                        ) : null}
                      </div>
                    ))
                  )}
                </div>
              </section>
            </>
          )}
        </div>
      </div>
    </section>
  )
}

function ActionButton({
  label,
  onClick,
  disabled,
  tone = 'default',
}: {
  label: string
  onClick: () => void
  disabled?: boolean
  tone?: 'default' | 'ok' | 'warn' | 'danger'
}) {
  let classes = 'border-slate-700 text-slate-300 hover:bg-slate-800'
  if (tone === 'ok') {
    classes = 'border-emerald-500/40 bg-emerald-500/10 text-emerald-300 hover:bg-emerald-500/20'
  } else if (tone === 'warn') {
    classes = 'border-orange-500/40 bg-orange-500/10 text-orange-300 hover:bg-orange-500/20'
  } else if (tone === 'danger') {
    classes = 'border-rose-500/40 bg-rose-500/10 text-rose-300 hover:bg-rose-500/20'
  }

  return (
    <button
      onClick={onClick}
      disabled={disabled}
      className={`rounded-md border px-2 py-1 text-xs transition disabled:opacity-60 ${classes}`}
      type="button"
    >
      {label}
    </button>
  )
}

function Stat({ label, value, color }: { label: string; value: number; color: string }) {
  return (
    <div className="rounded-xl border border-slate-800 bg-slate-900/50 px-4 py-3">
      <dt className="text-xs uppercase tracking-wider text-slate-500">{label}</dt>
      <dd className={`text-2xl font-bold ${color}`}>{value}</dd>
    </div>
  )
}
