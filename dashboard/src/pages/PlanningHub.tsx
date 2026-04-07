import { type FormEvent, useCallback, useEffect, useMemo, useState } from 'react'
import { getJson, patchJson, postJson } from '../lib/api'

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
    <section className="space-y-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Planning Hub</h1>
          <p className="mt-1 text-sm text-slate-400">
            Mission Control planning parity for epics, sprints, progress metrics, and work-item assignment.
          </p>
        </div>
        <button
          onClick={() => void loadCore()}
          disabled={loading || saving}
          className="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-60"
        >
          ↻ Refresh
        </button>
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

      <div className="grid gap-4 xl:grid-cols-2">
        <form onSubmit={createEpic} className="rounded-xl border border-slate-800 bg-slate-900/60 p-4 space-y-2">
          <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-300">Create epic</h2>
          <input
            value={newEpicTitle}
            onChange={(event) => setNewEpicTitle(event.target.value)}
            placeholder="Epic title"
            className="w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
            required
          />
          <input
            value={newEpicDescription}
            onChange={(event) => setNewEpicDescription(event.target.value)}
            placeholder="Description"
            className="w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
          />
          <div className="grid gap-2 sm:grid-cols-[1fr_auto]">
            <select
              value={newEpicStatus}
              onChange={(event) => setNewEpicStatus(event.target.value as EpicStatus)}
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
            >
              {EPIC_STATUSES.map((status) => (
                <option key={status} value={status}>
                  {labelize(status)}
                </option>
              ))}
            </select>
            <button
              type="submit"
              disabled={saving}
              className="rounded-md border border-sky-500/40 bg-sky-500/15 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/25 disabled:opacity-60"
            >
              Create epic
            </button>
          </div>
        </form>

        <form onSubmit={createSprint} className="rounded-xl border border-slate-800 bg-slate-900/60 p-4 space-y-2">
          <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-300">Create sprint</h2>
          <input
            value={newSprintName}
            onChange={(event) => setNewSprintName(event.target.value)}
            placeholder="Sprint name"
            className="w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
            required
          />
          <div className="grid gap-2 sm:grid-cols-2">
            <input
              type="date"
              value={newSprintStartDate}
              onChange={(event) => setNewSprintStartDate(event.target.value)}
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
            />
            <input
              type="date"
              value={newSprintEndDate}
              onChange={(event) => setNewSprintEndDate(event.target.value)}
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
            />
          </div>
          <input
            value={newSprintGoal}
            onChange={(event) => setNewSprintGoal(event.target.value)}
            placeholder="Sprint goal"
            className="w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
          />
          <button
            type="submit"
            disabled={saving}
            className="rounded-md border border-sky-500/40 bg-sky-500/15 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/25 disabled:opacity-60"
          >
            Create sprint
          </button>
        </form>
      </div>

      <div className="rounded-xl border border-slate-800 bg-slate-900/60 p-4 space-y-2">
        <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-300">Assign work item</h2>
        <div className="grid gap-2 md:grid-cols-4">
          <select
            value={assignmentWorkItemId}
            onChange={(event) => setAssignmentWorkItemId(event.target.value)}
            className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
          >
            <option value="">Select work item…</option>
            {items.map((item) => (
              <option key={item.id} value={item.id}>
                {item.title}
              </option>
            ))}
          </select>
          <select
            value={assignmentEpicId}
            onChange={(event) => setAssignmentEpicId(event.target.value)}
            className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
          >
            <option value="">No epic</option>
            {epics.map((epic) => (
              <option key={epic.id} value={epic.id}>
                {epic.title}
              </option>
            ))}
          </select>
          <select
            value={assignmentSprintId}
            onChange={(event) => setAssignmentSprintId(event.target.value)}
            className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 focus:border-sky-500 focus:outline-none"
          >
            <option value="">No sprint</option>
            {sprints.map((sprint) => (
              <option key={sprint.id} value={sprint.id}>
                {sprint.name}
              </option>
            ))}
          </select>
          <button
            onClick={() => void assignItem()}
            disabled={saving || !assignmentWorkItemId}
            className="rounded-md border border-sky-500/40 bg-sky-500/15 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/25 disabled:opacity-60"
          >
            Save assignment
          </button>
        </div>
      </div>

      <div className="grid gap-4 xl:grid-cols-2">
        <div className="space-y-4">
          <div className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
            <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">
              Epics ({epics.length})
            </h2>
            {epics.length === 0 ? (
              <p className="text-sm text-slate-500">No epics yet.</p>
            ) : (
              <div className="space-y-2">
                {epics.map((epic) => (
                  <div
                    key={epic.id}
                    className={`rounded-md border px-3 py-2 ${
                      epic.id === selectedEpicId
                        ? 'border-sky-500/40 bg-sky-500/10'
                        : 'border-slate-800 bg-slate-950/70'
                    }`}
                  >
                    <button
                      type="button"
                      onClick={() => setSelectedEpicId(epic.id)}
                      className="w-full text-left"
                    >
                      <p className="font-medium text-slate-100">{epic.title}</p>
                      <p className="text-xs text-slate-400">{labelize(String(epic.status))}</p>
                    </button>
                    <div className="mt-2 flex flex-wrap gap-1">
                      {EPIC_STATUSES.map((status) => (
                        <button
                          key={`${epic.id}-${status}`}
                          onClick={() => void updateEpicStatus(epic.id, status)}
                          disabled={saving}
                          className={`rounded-md border px-2 py-1 text-xs ${
                            String(epic.status) === status
                              ? 'border-sky-500/40 bg-sky-500/10 text-sky-300'
                              : 'border-slate-700 text-slate-300 hover:bg-slate-800'
                          }`}
                        >
                          {labelize(status)}
                        </button>
                      ))}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>

          <div className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
            <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">
              Epic progress detail
            </h2>
            {!selectedEpic ? (
              <p className="text-sm text-slate-500">Select an epic.</p>
            ) : !epicProgress ? (
              <p className="text-sm text-slate-500">No progress data available.</p>
            ) : (
              <div className="space-y-2">
                <p className="font-medium text-slate-100">{epicProgress.title}</p>
                <p className="text-sm text-slate-400">
                  {epicProgress.done_items} / {epicProgress.total_items} work items complete
                </p>
                <div className="h-2 overflow-hidden rounded-full bg-slate-800">
                  <div
                    className="h-full rounded-full bg-sky-500"
                    style={{ width: `${Math.max(0, Math.min(100, epicProgress.progress_pct))}%` }}
                  />
                </div>
                <p className="text-xs text-slate-500">{epicProgress.progress_pct.toFixed(1)}% complete</p>
              </div>
            )}
          </div>
        </div>

        <div className="space-y-4">
          <div className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
            <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">
              Sprints ({sprints.length})
            </h2>
            {sprints.length === 0 ? (
              <p className="text-sm text-slate-500">No sprints yet.</p>
            ) : (
              <div className="space-y-2">
                {sprints.map((sprint) => (
                  <button
                    key={sprint.id}
                    type="button"
                    onClick={() => setSelectedSprintId(sprint.id)}
                    className={`w-full rounded-md border px-3 py-2 text-left ${
                      sprint.id === selectedSprintId
                        ? 'border-sky-500/40 bg-sky-500/10'
                        : 'border-slate-800 bg-slate-950/70 hover:border-slate-600'
                    }`}
                  >
                    <p className="font-medium text-slate-100">{sprint.name}</p>
                    <p className="text-xs text-slate-400">
                      {fmtDate(sprint.start_date)} → {fmtDate(sprint.end_date)}
                    </p>
                    {sprint.goal ? <p className="mt-1 text-xs text-slate-500">{sprint.goal}</p> : null}
                  </button>
                ))}
              </div>
            )}
          </div>

          <div className="rounded-xl border border-slate-800 bg-slate-900/60 p-4 space-y-3">
            <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-300">Sprint stats + burndown</h2>
            {!selectedSprint ? (
              <p className="text-sm text-slate-500">Select a sprint.</p>
            ) : !sprintStats ? (
              <p className="text-sm text-slate-500">No sprint stats available.</p>
            ) : (
              <>
                <div className="grid grid-cols-2 gap-2 text-sm">
                  <Metric label="Total" value={sprintStats.total_items} />
                  <Metric label="Done" value={sprintStats.done_items} />
                  <Metric label="In Progress" value={sprintStats.in_progress_items} />
                  <Metric label="Blocked" value={sprintStats.blocked_items} />
                </div>
                <p className="text-xs text-slate-400">Velocity: {sprintStats.velocity.toFixed(2)}</p>

                {burndown.length === 0 ? (
                  <p className="text-xs text-slate-500">No burndown points yet.</p>
                ) : (
                  <div className="space-y-1">
                    {burndown.slice(-7).map((point, idx) => (
                      <div
                        key={`${point.date}-${idx}`}
                        className="flex items-center justify-between rounded-md border border-slate-800 bg-slate-950/70 px-3 py-1.5 text-xs"
                      >
                        <span className="text-slate-400">{fmtDate(point.date)}</span>
                        <span className="text-slate-300">
                          ideal {point.ideal_remaining.toFixed(1)} · actual {point.actual_remaining}
                        </span>
                      </div>
                    ))}
                  </div>
                )}
              </>
            )}
          </div>
        </div>
      </div>
    </section>
  )
}

function Metric({ label, value }: { label: string; value: number }) {
  return (
    <div className="rounded-lg border border-slate-800 bg-slate-950/70 px-3 py-2">
      <p className="text-xs uppercase tracking-wider text-slate-500">{label}</p>
      <p className="text-lg font-semibold text-slate-200">{value}</p>
    </div>
  )
}
