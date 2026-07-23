import { useCallback, useEffect, useMemo, useState } from 'react'
import { PanelHeader, RefreshButton } from '../components/PanelHeader'
import { StatusBadge } from '../components/StatusBadge'
import { Badge } from '../components/ui/badge'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { getJson } from '../lib/api'

type BuildStatus = 'idea' | 'ready' | 'building' | 'in_review' | 'done' | 'failed'

type WorkItem = {
  id: string
  title: string
  kind?: string | null
  status?: string | null
  priority?: string | number | null
  assigned_computer?: string | null
  branch_name?: string | null
  pr_url?: string | null
}

const STATUS_ORDER: BuildStatus[] = ['idea', 'ready', 'building', 'in_review', 'done', 'failed']

const STATUS_LABEL: Record<BuildStatus, string> = {
  idea: 'Idea',
  ready: 'Ready',
  building: 'Building',
  in_review: 'In Review',
  done: 'Done',
  failed: 'Failed',
}

const BUILDING_STATUSES = new Set(['building', 'claimed', 'in_progress'])

function normalizedStatus(status: string | null | undefined): BuildStatus {
  const value = (status ?? '').trim().toLowerCase()
  if (BUILDING_STATUSES.has(value)) return 'building'
  if (value === 'idea' || value === 'ready' || value === 'in_review' || value === 'done' || value === 'failed') {
    return value
  }
  return 'idea'
}

function priorityLabel(priority: WorkItem['priority']): string {
  if (priority == null || priority === '') return 'none'
  return String(priority)
}

function priorityRank(priority: WorkItem['priority']): number {
  const value = priorityLabel(priority).toLowerCase()
  if (['critical', 'urgent', 'p0', 'p1', '1'].includes(value)) return 0
  if (['high', 'p2', '2'].includes(value)) return 1
  if (['medium', 'normal', 'p3', '3'].includes(value)) return 2
  if (['low', 'p4', '4'].includes(value)) return 3
  return 4
}

export function BuildPipeline() {
  const [items, setItems] = useState<WorkItem[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const payload = await getJson<unknown>('/api/pm/work-items')
      setItems(Array.isArray(payload) ? (payload as WorkItem[]) : [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load build pipeline')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
    const id = window.setInterval(() => void load(), 15000)
    return () => window.clearInterval(id)
  }, [load])

  const grouped = useMemo(() => {
    const byStatus: Record<BuildStatus, WorkItem[]> = {
      idea: [],
      ready: [],
      building: [],
      in_review: [],
      done: [],
      failed: [],
    }

    for (const item of items) {
      byStatus[normalizedStatus(item.status)].push(item)
    }

    for (const status of STATUS_ORDER) {
      byStatus[status].sort((a, b) => {
        const priorityDiff = priorityRank(a.priority) - priorityRank(b.priority)
        if (priorityDiff !== 0) return priorityDiff
        return a.title.localeCompare(b.title)
      })
    }

    return byStatus
  }, [items])

  const liveBuilds = useMemo(
    () =>
      grouped.building
        .filter((item) => Boolean(item.assigned_computer?.trim()))
        .sort((a, b) =>
          (a.assigned_computer ?? '').localeCompare(b.assigned_computer ?? '') ||
          a.title.localeCompare(b.title),
        ),
    [grouped],
  )

  return (
    <section className="space-y-6 bg-background text-foreground">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight text-foreground">Build Pipeline</h1>
          <p className="mt-1 text-sm text-dim">
            Pillar-4 work items flowing from ideas to reviewed fleet builds.
          </p>
        </div>
      </div>

      {error ? (
        <div className="rounded-xl border border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">
          {error}
        </div>
      ) : null}

      <Card className="bg-surface">
        <PanelHeader
          title="Active Builds"
          subtitle={`${liveBuilds.length} assigned build${liveBuilds.length === 1 ? '' : 's'}`}
          rightSlot={<RefreshButton onClick={() => void load()} loading={loading} />}
        />

        {loading && items.length === 0 ? (
          <div className="mt-4 text-sm text-zinc-500">Loading...</div>
        ) : liveBuilds.length === 0 ? (
          <div className="mt-4 rounded-lg border border-border bg-panel px-3 py-2 text-sm text-dim">
            No assigned builds are running.
          </div>
        ) : (
          <div className="mt-4 flex gap-3 overflow-x-auto pb-1">
            {liveBuilds.map((item) => (
              <article
                key={item.id}
                className="min-w-64 max-w-80 rounded-lg border border-border bg-panel px-3 py-2"
              >
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <h3 className="truncate text-sm font-semibold text-foreground">
                      {item.assigned_computer}
                    </h3>
                    <p className="mt-1 line-clamp-2 text-xs leading-5 text-muted">{item.title}</p>
                  </div>
                  <StatusBadge status="building" tone="warn">
                    Building
                  </StatusBadge>
                </div>
              </article>
            ))}
          </div>
        )}
      </Card>

      {loading && items.length === 0 ? (
        <Card className="bg-surface">
          <p className="text-sm text-muted">Loading work items...</p>
        </Card>
      ) : null}

      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-6">
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
                  No work items
                </div>
              ) : (
                grouped[status].map((item) => (
                  <div
                    key={item.id}
                    className="rounded-lg border border-border bg-surface p-3 transition hover:border-border-subtle"
                  >
                    <div className="flex items-start justify-between gap-2">
                      <p className="min-w-0 wrap-break-word text-sm font-medium text-foreground">
                        {item.title}
                      </p>
                      <StatusBadge status={status}>{STATUS_LABEL[status]}</StatusBadge>
                    </div>

                    <dl className="mt-3 space-y-1 text-xs">
                      <BuildField label="Kind" value={item.kind || 'none'} />
                      <BuildField label="Priority" value={priorityLabel(item.priority)} />
                      <BuildField label="Node" value={item.assigned_computer || 'unassigned'} />
                      <BuildField label="Branch" value={item.branch_name || 'none'} />
                    </dl>

                    {item.pr_url ? (
                      <a
                        className="mt-3 inline-flex text-xs font-medium text-primary hover:text-primary-muted"
                        href={item.pr_url}
                        target="_blank"
                        rel="noreferrer"
                      >
                        Pull request
                      </a>
                    ) : null}
                  </div>
                ))
              )}
            </div>
          </Card>
        ))}
      </div>
    </section>
  )
}

function BuildField({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-start justify-between gap-2">
      <dt className="text-dim">{label}</dt>
      <dd className="min-w-0 wrap-break-word text-right text-muted">{value}</dd>
    </div>
  )
}
