import { type FormEvent, useCallback, useEffect, useMemo, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { getJson, patchJson, postJson } from '../lib/api'
import { cn } from '../lib/utils'

type EpicStatus = 'open' | 'in_progress' | 'done' | 'cancelled'

type Epic = {
  id: string
  title: string
  description: string
  status: EpicStatus | string
  updated_at: string
}

type EpicProgress = {
  id: string
  title: string
  description: string
  status: EpicStatus | string
  total_items: number
  done_items: number
  progress_pct: number
  work_item_ids: string[]
}

type Sprint = {
  id: string
  name: string
  start_date?: string | null
  end_date?: string | null
  goal: string
}

type SprintStats = {
  id: string
  name: string
  start_date?: string | null
  end_date?: string | null
  goal: string
  total_items: number
  done_items: number
  in_progress_items: number
  blocked_items: number
  velocity: number
  work_item_ids: string[]
}

type BurndownPoint = {
  date: string
  ideal_remaining: number
  actual_remaining: number
}

type WorkItem = {
  id: string
  title: string
  status: string
  epic_id?: string | null
  sprint_id?: string | null
}

const EPIC_STATUSES: EpicStatus[] = ['open', 'in_progress', 'done', 'cancelled']

const fieldClass =
  'w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-foreground outline-none transition placeholder:text-dim focus:border-primary disabled:cursor-not-allowed disabled:opacity-60'

const compactFieldClass =
  'rounded-lg border border-border bg-surface px-3 py-2 text-sm text-foreground outline-none transition placeholder:text-dim focus:border-primary disabled:cursor-not-allowed disabled:opacity-60'

function labelize(value: string): string {
  return value
    .replaceAll('_', ' ')
    .split(' ')
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(' ')
}

function fmtDate(value?: string | null): string {
  if (!value) return '—'
  try {
    return new Date(value).toLocaleDateString()
  } catch {
    return value
  }
}

export function PlanningHub() {
  const [epics, setEpics] = useState<Epic[]>([])
  const [sprints, setSprints] = useState<Sprint[]>([])
  const [items, setItems] = useState<WorkItem[]>([])

  const [selectedEpicId, setSelectedEpicId] = useState('')
  const [selectedSprintId, setSelectedSprintId] = useState('')

  const [epicProgress, setEpicProgress] = useState<EpicProgress | null>(null)
  const [sprintStats, setSprintStats] = useState<SprintStats | null>(null)
  const [burndown, setBurndown] = useState<BurndownPoint[]>([])

  const [newEpicTitle, setNewEpicTitle] = useState('')
  const [newEpicDescription, setNewEpicDescription] = useState('')
  const [newEpicStatus, setNewEpicStatus] = useState<EpicStatus>('open')

  const [newSprintName, setNewSprintName] = useState('')
  const [newSprintStartDate, setNewSprintStartDate] = useState('')
  const [newSprintEndDate, setNewSprintEndDate] = useState('')
  const [newSprintGoal, setNewSprintGoal] = useState('')

  const [assignmentWorkItemId, setAssignmentWorkItemId] = useState('')
  const [assignmentEpicId, setAssignmentEpicId] = useState('')
  const [assignmentSprintId, setAssignmentSprintId] = useState('')

  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)

  const selectedEpic = useMemo(
    () => epics.find((epic) => epic.id === selectedEpicId) ?? null,
    [epics, selectedEpicId],
  )

  const selectedSprint = useMemo(
    () => sprints.find((sprint) => sprint.id === selectedSprintId) ?? null,
    [selectedSprintId, sprints],
  )

  const loadCore = useCallback(async () => {
    try {
      setError(null)
      const [epicsPayload, sprintsPayload, workItemsPayload] = await Promise.all([
        getJson<unknown>('/api/mc/epics'),
        getJson<unknown>('/api/mc/sprints'),
        getJson<unknown>('/api/mc/work-items'),
      ])

      const nextEpics = Array.isArray(epicsPayload) ? (epicsPayload as Epic[]) : []
      const nextSprints = Array.isArray(sprintsPayload) ? (sprintsPayload as Sprint[]) : []
      const nextItems = Array.isArray(workItemsPayload) ? (workItemsPayload as WorkItem[]) : []

      setEpics(nextEpics)
      setSprints(nextSprints)
      setItems(nextItems)

      if (nextEpics.length === 0) {
        setSelectedEpicId('')
      } else if (!nextEpics.some((epic) => epic.id === selectedEpicId)) {
        setSelectedEpicId(nextEpics[0].id)
      }

      if (nextSprints.length === 0) {
        setSelectedSprintId('')
      } else if (!nextSprints.some((sprint) => sprint.id === selectedSprintId)) {
        setSelectedSprintId(nextSprints[0].id)
      }

      if (nextItems.length > 0 && !assignmentWorkItemId) {
        const candidate = nextItems.find((item) => item.status !== 'done') ?? nextItems[0]
        setAssignmentWorkItemId(candidate.id)
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load planning data')
    } finally {
      setLoading(false)
    }
  }, [assignmentWorkItemId, selectedEpicId, selectedSprintId])

  const loadEpicProgress = useCallback(async (epicId: string) => {
    if (!epicId) {
      setEpicProgress(null)
      return
    }

    try {
      const payload = await getJson<EpicProgress>(`/api/mc/epics/${epicId}/progress`)
      setEpicProgress(payload)
    } catch {
      setEpicProgress(null)
    }
  }, [])

  const loadSprintStats = useCallback(async (sprintId: string) => {
    if (!sprintId) {
      setSprintStats(null)
      setBurndown([])
      return
    }

    try {
      const [statsPayload, burndownPayload] = await Promise.all([
        getJson<SprintStats>(`/api/mc/sprints/${sprintId}/stats`),
        getJson<unknown>(`/api/mc/sprints/${sprintId}/burndown`),
      ])
      setSprintStats(statsPayload)
      setBurndown(Array.isArray(burndownPayload) ? (burndownPayload as BurndownPoint[]) : [])
    } catch {
      setSprintStats(null)
      setBurndown([])
    }
  }, [])

  useEffect(() => {
    void loadCore()
  }, [loadCore])

  useEffect(() => {
    void loadEpicProgress(selectedEpicId)
  }, [loadEpicProgress, selectedEpicId])

  useEffect(() => {
    void loadSprintStats(selectedSprintId)
  }, [loadSprintStats, selectedSprintId])

  const createEpic = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!newEpicTitle.trim()) return

    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await postJson('/api/mc/epics', {
        title: newEpicTitle.trim(),
        description: newEpicDescription.trim(),
        status: newEpicStatus,
      })

      setNewEpicTitle('')
      setNewEpicDescription('')
      setNewEpicStatus('open')
      setNotice('Epic created')
      await loadCore()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create epic')
    } finally {
      setSaving(false)
    }
  }

  const createSprint = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!newSprintName.trim()) return

    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await postJson('/api/mc/sprints', {
        name: newSprintName.trim(),
        start_date: newSprintStartDate || undefined,
        end_date: newSprintEndDate || undefined,
        goal: newSprintGoal.trim(),
      })

      setNewSprintName('')
      setNewSprintStartDate('')
      setNewSprintEndDate('')
      setNewSprintGoal('')
      setNotice('Sprint created')
      await loadCore()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create sprint')
    } finally {
      setSaving(false)
    }
  }

  const assignItem = async () => {
    if (!assignmentWorkItemId) return

    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await patchJson(`/api/mc/work-items/${assignmentWorkItemId}`, {
        epic_id: assignmentEpicId || null,
        sprint_id: assignmentSprintId || null,
      })

      setNotice('Work item assignment updated')
      await loadCore()
      if (selectedEpicId) await loadEpicProgress(selectedEpicId)
      if (selectedSprintId) await loadSprintStats(selectedSprintId)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update item assignment')
    } finally {
      setSaving(false)
    }
  }

  const updateEpicStatus = async (epicId: string, status: EpicStatus) => {
    try {
      setSaving(true)
      setError(null)
      setNotice(null)

      await patchJson(`/api/mc/epics/${epicId}`, { status })
      setNotice('Epic status updated')
      await loadCore()
      await loadEpicProgress(epicId)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update epic status')
    } finally {
      setSaving(false)
    }
  }

  return (
    <section className="min-h-full space-y-6 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Planning Hub</h1>
            {loading ? <Badge variant="info">loading</Badge> : saving ? <Badge variant="warn">saving</Badge> : null}
          </div>
          <p className="mt-1 text-sm text-dim">
            Mission Control planning parity for epics, sprints, progress metrics, and work-item assignment.
          </p>
        </div>
        <Button
          type="button"
          variant="outline"
          onClick={() => void loadCore()}
          disabled={loading || saving}
        >
          Refresh
        </Button>
      </div>

      {error ? (
        <Card className="border-border bg-panel">
          <p className="text-sm text-status-crit">{error}</p>
        </Card>
      ) : null}
      {notice ? (
        <Card className="border-border bg-panel">
          <p className="text-sm text-status-ok">{notice}</p>
        </Card>
      ) : null}

      <div className="grid gap-4 md:grid-cols-3">
        <Metric label="Epics" value={epics.length} detail={`${items.filter((item) => item.epic_id).length} assigned`} />
        <Metric
          label="Sprints"
          value={sprints.length}
          detail={`${items.filter((item) => item.sprint_id).length} scheduled`}
        />
        <Metric
          label="Work Items"
          value={items.length}
          detail={`${items.filter((item) => item.status === 'done').length} done`}
        />
      </div>

      <div className="grid gap-4 xl:grid-cols-2">
        <form onSubmit={createEpic}>
          <Card className="space-y-3 bg-panel">
            <CardHeader className="items-start gap-3">
              <div>
                <CardTitle>Create Epic</CardTitle>
                <CardDescription>Define a planning container and starting state</CardDescription>
              </div>
              <Badge variant="neutral">{EPIC_STATUSES.length} states</Badge>
            </CardHeader>
            <input
              aria-label="Epic title"
              value={newEpicTitle}
              onChange={(event) => setNewEpicTitle(event.target.value)}
              placeholder="Epic title"
              className={fieldClass}
              required
            />
            <input
              aria-label="Epic description"
              value={newEpicDescription}
              onChange={(event) => setNewEpicDescription(event.target.value)}
              placeholder="Description"
              className={fieldClass}
            />
            <div className="grid gap-2 sm:grid-cols-[1fr_auto]">
              <select
                aria-label="Epic status"
                value={newEpicStatus}
                onChange={(event) => setNewEpicStatus(event.target.value as EpicStatus)}
                className={compactFieldClass}
              >
                {EPIC_STATUSES.map((status) => (
                  <option key={status} value={status}>
                    {labelize(status)}
                  </option>
                ))}
              </select>
              <Button type="submit" disabled={saving}>
                Create epic
              </Button>
            </div>
          </Card>
        </form>

        <form onSubmit={createSprint}>
          <Card className="space-y-3 bg-panel">
            <CardHeader className="items-start">
              <div>
                <CardTitle>Create Sprint</CardTitle>
                <CardDescription>Set a sprint window and goal for scheduled work</CardDescription>
              </div>
            </CardHeader>
            <input
              aria-label="Sprint name"
              value={newSprintName}
              onChange={(event) => setNewSprintName(event.target.value)}
              placeholder="Sprint name"
              className={fieldClass}
              required
            />
            <div className="grid gap-2 sm:grid-cols-2">
              <input
                aria-label="Sprint start date"
                type="date"
                value={newSprintStartDate}
                onChange={(event) => setNewSprintStartDate(event.target.value)}
                className={compactFieldClass}
              />
              <input
                aria-label="Sprint end date"
                type="date"
                value={newSprintEndDate}
                onChange={(event) => setNewSprintEndDate(event.target.value)}
                className={compactFieldClass}
              />
            </div>
            <input
              aria-label="Sprint goal"
              value={newSprintGoal}
              onChange={(event) => setNewSprintGoal(event.target.value)}
              placeholder="Sprint goal"
              className={fieldClass}
            />
            <Button type="submit" disabled={saving} className="w-full sm:w-fit">
              Create sprint
            </Button>
          </Card>
        </form>
      </div>

      <Card className="space-y-3 bg-surface">
        <CardHeader className="items-start gap-3">
          <div>
            <CardTitle>Assign Work Item</CardTitle>
            <CardDescription>Move work between epics and sprints without changing item status</CardDescription>
          </div>
          <Badge variant="neutral">{items.length} items</Badge>
        </CardHeader>
        <div className="grid gap-2 md:grid-cols-4">
          <select
            aria-label="Work item"
            value={assignmentWorkItemId}
            onChange={(event) => setAssignmentWorkItemId(event.target.value)}
            className={compactFieldClass}
          >
            <option value="">Select work item…</option>
            {items.map((item) => (
              <option key={item.id} value={item.id}>
                {item.title}
              </option>
            ))}
          </select>
          <select
            aria-label="Epic assignment"
            value={assignmentEpicId}
            onChange={(event) => setAssignmentEpicId(event.target.value)}
            className={compactFieldClass}
          >
            <option value="">No epic</option>
            {epics.map((epic) => (
              <option key={epic.id} value={epic.id}>
                {epic.title}
              </option>
            ))}
          </select>
          <select
            aria-label="Sprint assignment"
            value={assignmentSprintId}
            onChange={(event) => setAssignmentSprintId(event.target.value)}
            className={compactFieldClass}
          >
            <option value="">No sprint</option>
            {sprints.map((sprint) => (
              <option key={sprint.id} value={sprint.id}>
                {sprint.name}
              </option>
            ))}
          </select>
          <Button
            type="button"
            onClick={() => void assignItem()}
            disabled={saving || !assignmentWorkItemId}
          >
            Save assignment
          </Button>
        </div>
      </Card>

      <div className="grid gap-4 xl:grid-cols-2">
        <div className="space-y-4">
          <Card className="bg-panel">
            <CardHeader>
              <div>
                <CardTitle>Epics</CardTitle>
                <CardDescription>Current planning containers and status controls</CardDescription>
              </div>
              <Badge variant="neutral">{epics.length}</Badge>
            </CardHeader>
            {epics.length === 0 ? (
              <p className="text-sm text-dim">No epics yet.</p>
            ) : (
              <div className="space-y-2">
                {epics.map((epic) => (
                  <div
                    key={epic.id}
                    className={cn(
                      'rounded-lg border px-3 py-3 transition',
                      epic.id === selectedEpicId
                        ? 'border-primary bg-primary-subtle'
                        : 'border-border bg-surface hover:border-border-subtle hover:bg-elevated',
                    )}
                  >
                    <button
                      type="button"
                      onClick={() => setSelectedEpicId(epic.id)}
                      className="w-full text-left"
                    >
                      <div className="flex flex-wrap items-start justify-between gap-2">
                        <p className="font-medium text-foreground">{epic.title}</p>
                        <StatusBadge status={String(epic.status)}>{labelize(String(epic.status))}</StatusBadge>
                      </div>
                      {epic.description ? <p className="mt-1 text-xs text-muted">{epic.description}</p> : null}
                    </button>
                    <div className="mt-3 flex flex-wrap gap-1.5">
                      {EPIC_STATUSES.map((status) => (
                        <Button
                          key={`${epic.id}-${status}`}
                          type="button"
                          variant="outline"
                          size="sm"
                          onClick={() => void updateEpicStatus(epic.id, status)}
                          disabled={saving}
                          className={cn(
                            String(epic.status) === status
                              ? 'border-primary bg-primary-subtle text-primary'
                              : 'border-border-subtle text-muted hover:bg-elevated hover:text-foreground',
                          )}
                        >
                          {labelize(status)}
                        </Button>
                      ))}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </Card>

          <Card className="bg-panel">
            <CardHeader>
              <div>
                <CardTitle>Epic Progress</CardTitle>
                <CardDescription>Completion detail for the selected epic</CardDescription>
              </div>
            </CardHeader>
            {!selectedEpic ? (
              <p className="text-sm text-dim">Select an epic.</p>
            ) : !epicProgress ? (
              <p className="text-sm text-dim">No progress data available.</p>
            ) : (
              <div className="space-y-2">
                <div className="flex flex-wrap items-start justify-between gap-2">
                  <p className="font-medium text-foreground">{epicProgress.title}</p>
                  <StatusBadge status={String(epicProgress.status)}>{labelize(String(epicProgress.status))}</StatusBadge>
                </div>
                <p className="text-sm text-muted">
                  {epicProgress.done_items} / {epicProgress.total_items} work items complete
                </p>
                <div className="h-2 overflow-hidden rounded-full bg-elevated">
                  <div
                    className="h-full rounded-full bg-primary"
                    style={{ width: `${Math.max(0, Math.min(100, epicProgress.progress_pct))}%` }}
                  />
                </div>
                <p className="text-xs text-dim">{epicProgress.progress_pct.toFixed(1)}% complete</p>
              </div>
            )}
          </Card>
        </div>

        <div className="space-y-4">
          <Card className="bg-panel">
            <CardHeader>
              <div>
                <CardTitle>Sprints</CardTitle>
                <CardDescription>Timeboxed goals and selected sprint context</CardDescription>
              </div>
              <Badge variant="neutral">{sprints.length}</Badge>
            </CardHeader>
            {sprints.length === 0 ? (
              <p className="text-sm text-dim">No sprints yet.</p>
            ) : (
              <div className="space-y-2">
                {sprints.map((sprint) => (
                  <button
                    key={sprint.id}
                    type="button"
                    onClick={() => setSelectedSprintId(sprint.id)}
                    className={cn(
                      'w-full rounded-lg border px-3 py-3 text-left transition',
                      sprint.id === selectedSprintId
                        ? 'border-primary bg-primary-subtle'
                        : 'border-border bg-surface hover:border-border-subtle hover:bg-elevated',
                    )}
                  >
                    <p className="font-medium text-foreground">{sprint.name}</p>
                    <p className="text-xs text-muted">
                      {fmtDate(sprint.start_date)} → {fmtDate(sprint.end_date)}
                    </p>
                    {sprint.goal ? <p className="mt-1 text-xs text-dim">{sprint.goal}</p> : null}
                  </button>
                ))}
              </div>
            )}
          </Card>

          <Card className="space-y-3 bg-panel">
            <CardHeader>
              <div>
                <CardTitle>Sprint Stats + Burndown</CardTitle>
                <CardDescription>Throughput and recent remaining-work points</CardDescription>
              </div>
            </CardHeader>
            {!selectedSprint ? (
              <p className="text-sm text-dim">Select a sprint.</p>
            ) : !sprintStats ? (
              <p className="text-sm text-dim">No sprint stats available.</p>
            ) : (
              <>
                <div className="grid grid-cols-2 gap-2 text-sm">
                  <Metric label="Total" value={sprintStats.total_items} />
                  <Metric label="Done" value={sprintStats.done_items} />
                  <Metric label="In Progress" value={sprintStats.in_progress_items} />
                  <Metric label="Blocked" value={sprintStats.blocked_items} />
                </div>
                <p className="text-xs text-muted">Velocity: {sprintStats.velocity.toFixed(2)}</p>

                {burndown.length === 0 ? (
                  <p className="text-xs text-dim">No burndown points yet.</p>
                ) : (
                  <div className="space-y-1">
                    {burndown.slice(-7).map((point, idx) => (
                      <div
                        key={`${point.date}-${idx}`}
                        className="flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-1.5 text-xs"
                      >
                        <span className="text-dim">{fmtDate(point.date)}</span>
                        <span className="text-muted">
                          ideal {point.ideal_remaining.toFixed(1)} · actual {point.actual_remaining}
                        </span>
                      </div>
                    ))}
                  </div>
                )}
              </>
            )}
          </Card>
        </div>
      </div>
    </section>
  )
}

function Metric({ label, value, detail }: { label: string; value: number; detail?: string }) {
  return (
    <div className="rounded-lg border border-border bg-surface px-3 py-2">
      <p className="text-xs font-medium uppercase tracking-wider text-dim">{label}</p>
      <p className="text-lg font-semibold text-foreground">{value}</p>
      {detail ? <p className="text-xs text-muted">{detail}</p> : null}
    </div>
  )
}
