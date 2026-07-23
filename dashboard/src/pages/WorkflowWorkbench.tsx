import { type FormEvent, type ReactNode, useCallback, useEffect, useMemo, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { deleteJson, getJson, patchJson, postJson } from '../lib/api'
import { cn } from '../lib/utils'

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

const fieldClass =
  'w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-foreground outline-hidden transition placeholder:text-dim focus:border-primary disabled:cursor-not-allowed disabled:opacity-60'

const rowClass = 'rounded-lg border border-border bg-surface'

function asPriority(priority: WorkItem['priority']): number {
  if (typeof priority === 'number') return priority
  if (priority && typeof priority === 'object' && typeof priority.value === 'number') {
    return priority.value
  }
  return 3
}

function statusForBadge(status: string): string {
  switch (status) {
    case 'done':
    case 'approved':
      return 'success'
    case 'blocked':
    case 'changes_requested':
      return 'error'
    case 'review':
      return 'in_review'
    case 'in_progress':
      return 'running'
    case 'backlog':
    case 'todo':
    case 'pending':
      return 'pending'
    default:
      return status
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
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight text-foreground">Workflow Workbench</h1>
          <p className="mt-1 text-sm text-muted">
            Mission Control workflow parity for review lifecycle, dependencies, and task-group sequencing.
          </p>
        </div>
        <Button type="button" onClick={() => void refreshAll()} disabled={loading || saving}>
          Refresh
        </Button>
      </div>

      <div className="grid gap-3 sm:grid-cols-3">
        <Stat label="Open items" value={openCount} tone="info" />
        <Stat label="In review" value={reviewCount} tone="warn" />
        <Stat label="Blocked" value={blockedCount} tone="crit" />
      </div>

      {error ? (
        <Card className="border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">{error}</Card>
      ) : null}
      {notice ? (
        <Card className="border-status-ok bg-panel px-4 py-3 text-sm text-status-ok">{notice}</Card>
      ) : null}

      <Card className="bg-panel">
        <div className="grid gap-2 md:grid-cols-4">
          <select
            aria-label="Status filter"
            value={statusFilter}
            onChange={(event) => setStatusFilter(event.target.value as WorkItemStatus | '')}
            className={fieldClass}
          >
            {STATUS_FILTERS.map((opt) => (
              <option key={opt.label} value={opt.value}>
                {opt.label}
              </option>
            ))}
          </select>
          <input
            aria-label="Assignee filter"
            value={assigneeFilter}
            onChange={(event) => setAssigneeFilter(event.target.value)}
            placeholder="Assignee filter"
            className={fieldClass}
          />
          <Button type="button" variant="outline" onClick={() => void loadCore()}>
            Apply filters
          </Button>
          <Button
            type="button"
            variant="ghost"
            onClick={() => {
              setStatusFilter('')
              setAssigneeFilter('')
            }}
          >
            Clear filters
          </Button>
        </div>
      </Card>

      <div className="grid gap-4 xl:grid-cols-12">
        <div className="xl:col-span-5">
          <Card className="bg-panel">
            <CardHeader>
              <div>
                <CardTitle>Work Items</CardTitle>
                <CardDescription>{items.length} item{items.length === 1 ? '' : 's'} loaded</CardDescription>
              </div>
            </CardHeader>
            {loading ? (
              <EmptyState>Loading work items...</EmptyState>
            ) : items.length === 0 ? (
              <EmptyState>No work items match this filter.</EmptyState>
            ) : (
              <div className="space-y-2">
                {items.map((item) => (
                  <button
                    key={item.id}
                    type="button"
                    onClick={() => setSelectedItemId(item.id)}
                    className={cn(
                      'w-full rounded-lg border p-3 text-left transition',
                      item.id === selectedItemId
                        ? 'border-primary bg-primary-subtle'
                        : 'border-border bg-surface hover:border-border-subtle hover:bg-elevated',
                    )}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <p className="min-w-0 font-medium text-foreground">{item.title}</p>
                      <Badge variant="neutral" className="shrink-0">
                        P{asPriority(item.priority)}
                      </Badge>
                    </div>
                    <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-muted">
                      <StatusBadge status={statusForBadge(String(item.status))}>
                        {labelize(String(item.status))}
                      </StatusBadge>
                      <span>{item.assignee || 'unassigned'}</span>
                      <span>updated {fmtDate(item.updated_at)}</span>
                    </div>
                    {item.labels.length > 0 ? (
                      <div className="mt-2 flex flex-wrap gap-1">
                        {item.labels.map((label) => (
                          <Badge key={`${item.id}-${label}`} variant="default">
                            {label}
                          </Badge>
                        ))}
                      </div>
                    ) : null}
                  </button>
                ))}
              </div>
            )}
          </Card>
        </div>

        <div className="space-y-4 xl:col-span-7">
          {!selectedItem ? (
            <Card className="bg-panel py-5 text-sm text-muted">Select a work item to manage workflow actions.</Card>
          ) : (
            <>
              <Card className="bg-panel">
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <h2 className="text-lg font-semibold text-foreground">{selectedItem.title}</h2>
                      <StatusBadge status={statusForBadge(String(selectedItem.status))}>
                        {labelize(String(selectedItem.status))}
                      </StatusBadge>
                    </div>
                    <p className="mt-1 text-xs text-dim">ID: {selectedItem.id}</p>
                    {selectedItem.description ? (
                      <p className="mt-2 text-sm text-muted">{selectedItem.description}</p>
                    ) : null}
                    <div className="mt-3 flex flex-wrap gap-2">
                      <Badge variant="neutral">Assignee: {selectedItem.assignee || 'unassigned'}</Badge>
                      <Badge variant="neutral">Priority: P{asPriority(selectedItem.priority)}</Badge>
                      {selectedItem.sequence_order != null ? (
                        <Badge variant="neutral">Sequence: {selectedItem.sequence_order}</Badge>
                      ) : null}
                    </div>
                  </div>
                </div>

                <div className="mt-4 flex flex-wrap gap-2">
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
              </Card>

              <Card className="bg-panel">
                <CardHeader>
                  <div>
                    <CardTitle>Dependencies</CardTitle>
                    <CardDescription>
                      {loadingDetails
                        ? 'Loading dependency state...'
                        : depCheck
                          ? depCheck.can_start
                            ? 'Ready to start: no open blockers.'
                            : `${depCheck.blocked_count} blocker(s) still open.`
                          : 'No dependency state loaded.'}
                    </CardDescription>
                  </div>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => selectedItemId && void loadDetails(selectedItemId)}
                  >
                    Recheck
                  </Button>
                </CardHeader>

                {depCheck ? (
                  <StatusBadge status={depCheck.can_start ? 'ready' : 'error'}>
                    {depCheck.can_start ? 'Can start' : 'Blocked'}
                  </StatusBadge>
                ) : null}

                <div className="mt-3 grid gap-2 md:grid-cols-[1fr_auto]">
                  <select
                    aria-label="Dependency work item"
                    value={newDependencyId}
                    onChange={(event) => setNewDependencyId(event.target.value)}
                    className={fieldClass}
                  >
                    <option value="">Select dependency work item...</option>
                    {items
                      .filter((item) => item.id !== selectedItem.id)
                      .map((item) => (
                        <option key={item.id} value={item.id}>
                          {item.title}
                        </option>
                      ))}
                  </select>
                  <Button
                    type="button"
                    onClick={() => void addDependency()}
                    disabled={saving || !newDependencyId}
                    className="whitespace-nowrap"
                  >
                    Add dependency
                  </Button>
                </div>

                <div className="mt-3 space-y-2">
                  {dependencies.length === 0 ? (
                    <EmptyState>No dependencies configured.</EmptyState>
                  ) : (
                    dependencies.map((dep) => {
                      const target = itemById.get(dep.depends_on_id)
                      const blocked = depCheck?.blocked_by_ids.includes(dep.depends_on_id) ?? false
                      return (
                        <div
                          key={`${dep.work_item_id}-${dep.depends_on_id}`}
                          className={cn(rowClass, 'flex items-center justify-between gap-3 px-3 py-2')}
                        >
                          <div className="min-w-0">
                            <p className="text-sm text-foreground">{target?.title ?? dep.depends_on_id}</p>
                            <p className="text-xs text-dim">
                              {blocked ? 'Blocking' : 'Resolved'} / {fmtDate(dep.created_at)}
                            </p>
                          </div>
                          <div className="flex shrink-0 items-center gap-2">
                            <StatusBadge status={blocked ? 'error' : 'success'}>
                              {blocked ? 'Blocking' : 'Resolved'}
                            </StatusBadge>
                            <Button
                              type="button"
                              variant="outline"
                              size="sm"
                              onClick={() => void removeDependency(dep.depends_on_id)}
                            >
                              Remove
                            </Button>
                          </div>
                        </div>
                      )
                    })
                  )}
                </div>
              </Card>

              <Card className="bg-panel">
                <CardHeader>
                  <div>
                    <CardTitle>Review Checklist</CardTitle>
                    <CardDescription>{reviews.length} review item{reviews.length === 1 ? '' : 's'}</CardDescription>
                  </div>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => void resetReviewChecklist()}
                    disabled={saving || reviews.length === 0}
                  >
                    Reset all
                  </Button>
                </CardHeader>

                <form onSubmit={createReviewItem} className="grid gap-2 md:grid-cols-6">
                  <input
                    aria-label="Checklist title"
                    value={newReviewTitle}
                    onChange={(event) => setNewReviewTitle(event.target.value)}
                    placeholder="Checklist title"
                    className={cn(fieldClass, 'md:col-span-2')}
                    required
                  />
                  <input
                    aria-label="Reviewer"
                    value={newReviewer}
                    onChange={(event) => setNewReviewer(event.target.value)}
                    placeholder="Reviewer"
                    className={fieldClass}
                  />
                  <input
                    aria-label="Review notes"
                    value={newReviewNotes}
                    onChange={(event) => setNewReviewNotes(event.target.value)}
                    placeholder="Notes"
                    className={cn(fieldClass, 'md:col-span-2')}
                  />
                  <Button type="submit" disabled={saving}>
                    Add
                  </Button>
                </form>

                <div className="mt-3 space-y-2">
                  {reviews.length === 0 ? (
                    <EmptyState>No checklist items yet.</EmptyState>
                  ) : (
                    reviews.map((item) => (
                      <div key={item.id} className={cn(rowClass, 'p-3')}>
                        <div className="flex flex-wrap items-center justify-between gap-2">
                          <p className="text-sm text-foreground">{item.title}</p>
                          <div className="flex items-center gap-2">
                            <StatusBadge status={statusForBadge(String(item.status))}>
                              {labelize(String(item.status))}
                            </StatusBadge>
                            <Button
                              type="button"
                              variant="outline"
                              size="sm"
                              onClick={() => void removeReviewItem(item.id)}
                            >
                              Delete
                            </Button>
                          </div>
                        </div>
                        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-muted">
                          <span>Reviewer: {item.reviewer || 'unassigned'}</span>
                          <span>Updated: {fmtDate(item.updated_at)}</span>
                        </div>
                        {item.notes ? <p className="mt-1 text-xs text-dim">{item.notes}</p> : null}
                        <div className="mt-2 flex flex-wrap gap-1">
                          {REVIEW_STATUSES.map((status) => (
                            <Button
                              key={`${item.id}-${status}`}
                              type="button"
                              variant={String(item.status) === status ? 'default' : 'outline'}
                              size="sm"
                              onClick={() => void updateReviewStatus(item.id, status)}
                              disabled={saving}
                            >
                              {labelize(status)}
                            </Button>
                          ))}
                        </div>
                      </div>
                    ))
                  )}
                </div>
              </Card>

              <Card className="bg-panel">
                <CardHeader>
                  <div>
                    <CardTitle>Task Groups</CardTitle>
                    <CardDescription>Create groups and sequence the selected work item.</CardDescription>
                  </div>
                </CardHeader>

                <form onSubmit={createTaskGroup} className="grid gap-2 md:grid-cols-4">
                  <input
                    aria-label="Task group name"
                    value={newGroupName}
                    onChange={(event) => setNewGroupName(event.target.value)}
                    placeholder="New task group"
                    className={fieldClass}
                    required
                  />
                  <input
                    aria-label="Task group description"
                    value={newGroupDescription}
                    onChange={(event) => setNewGroupDescription(event.target.value)}
                    placeholder="Description"
                    className={cn(fieldClass, 'md:col-span-2')}
                  />
                  <Button type="submit" disabled={saving}>
                    Create
                  </Button>
                </form>

                <div className="mt-3 grid gap-2 md:grid-cols-[1fr_120px_auto_auto]">
                  <select
                    aria-label="Task group selection"
                    value={groupSelection}
                    onChange={(event) => setGroupSelection(event.target.value)}
                    className={fieldClass}
                  >
                    <option value="">Select task group...</option>
                    {groups.map((group) => (
                      <option key={group.id} value={group.id}>
                        {group.name}
                      </option>
                    ))}
                  </select>
                  <input
                    aria-label="Sequence order"
                    value={sequenceOrder}
                    onChange={(event) => setSequenceOrder(event.target.value)}
                    inputMode="numeric"
                    placeholder="Seq"
                    className={fieldClass}
                  />
                  <Button
                    type="button"
                    onClick={() => void assignToTaskGroup()}
                    disabled={saving || !groupSelection}
                    className="whitespace-nowrap"
                  >
                    Assign selected
                  </Button>
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => void unassignTaskGroup()}
                    disabled={saving || !selectedItem.task_group_id}
                  >
                    Unassign
                  </Button>
                </div>

                <div className="mt-3 space-y-2">
                  {groups.length === 0 ? (
                    <EmptyState>No task groups yet.</EmptyState>
                  ) : (
                    groups.map((group) => (
                      <div
                        key={group.id}
                        className={cn(
                          rowClass,
                          'px-3 py-2',
                          selectedItem.task_group_id === group.id && 'border-primary bg-primary-subtle',
                        )}
                      >
                        <div className="flex flex-wrap items-center justify-between gap-2">
                          <p className="text-sm text-foreground">{group.name}</p>
                          {selectedItem.task_group_id === group.id ? (
                            <Badge variant="default">Selected item</Badge>
                          ) : null}
                        </div>
                        {group.description ? <p className="text-xs text-dim">{group.description}</p> : null}
                      </div>
                    ))
                  )}
                </div>
              </Card>
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
  return (
    <Button
      type="button"
      variant="outline"
      size="sm"
      onClick={onClick}
      disabled={disabled}
      className={cn(
        tone === 'ok' && 'border-status-ok text-status-ok hover:bg-elevated',
        tone === 'warn' && 'border-status-warn text-status-warn hover:bg-elevated',
        tone === 'danger' && 'border-status-crit text-status-crit hover:bg-elevated',
      )}
    >
      {label}
    </Button>
  )
}

function Stat({ label, value, tone }: { label: string; value: number; tone: 'info' | 'warn' | 'crit' }) {
  return (
    <Card className="bg-panel px-4 py-3">
      <dt className="text-xs uppercase tracking-wider text-dim">{label}</dt>
      <dd
        className={cn(
          'text-2xl font-bold',
          tone === 'info' && 'text-status-info',
          tone === 'warn' && 'text-status-warn',
          tone === 'crit' && 'text-status-crit',
        )}
      >
        {value}
      </dd>
    </Card>
  )
}

function EmptyState({ children }: { children: ReactNode }) {
  return <p className="rounded-lg border border-border bg-surface px-3 py-2 text-sm text-dim">{children}</p>
}
