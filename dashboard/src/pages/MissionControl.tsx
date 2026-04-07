import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'
import { parseBoard } from '../lib/normalizers'
import type { MissionColumn } from '../types'

type WorkItemLike = {
  id?: string
  title?: string
  status?: string
  assignee?: string
  owner?: string
  priority?: number | string
}

const STATUS_ORDER = ['backlog', 'todo', 'in_progress', 'review', 'done', 'blocked'] as const

function statusTitle(status: string): string {
  switch (status) {
    case 'in_progress':
      return 'In Progress'
    default:
      return status
        .split('_')
        .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
        .join(' ')
  }
}

function boardFromWorkItems(payload: unknown): MissionColumn[] {
  if (!Array.isArray(payload)) return []

  const buckets = new Map<string, MissionColumn['cards']>()
  for (const status of STATUS_ORDER) {
    buckets.set(status, [])
  }

  for (const raw of payload) {
    if (typeof raw !== 'object' || raw === null) continue
    const item = raw as WorkItemLike
    const normalizedStatus = (item.status ?? 'backlog').toLowerCase().replace(/-/g, '_')
    const status = STATUS_ORDER.includes(normalizedStatus as (typeof STATUS_ORDER)[number])
      ? normalizedStatus
      : 'backlog'

    const list = buckets.get(status)
    if (!list) continue

    list.push({
      id: String(item.id ?? `${status}-${list.length}`),
      title: item.title?.trim() || 'Untitled work item',
      owner: item.owner ?? item.assignee,
      priority:
        typeof item.priority === 'number' ? String(item.priority) : item.priority,
      status,
    })
  }

  return STATUS_ORDER.map((status) => ({
    id: status,
    title: statusTitle(status),
    cards: buckets.get(status) ?? [],
  }))
}

export function MissionControl() {
  const [columns, setColumns] = useState<MissionColumn[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const payload = await getJson<unknown>('/api/mc/work-items').catch(() =>
        getJson<unknown>('/api/mc/board'),
      )

      if (Array.isArray(payload)) {
        setColumns(boardFromWorkItems(payload))
      } else {
        setColumns(parseBoard(payload))
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load mission board')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-xl font-semibold text-slate-100">Mission Control</h2>
        <button
          onClick={() => void load()}
          className="rounded-md border border-slate-700 bg-slate-900 px-3 py-1.5 text-sm text-slate-200 hover:border-slate-500"
          type="button"
        >
          Refresh Board
        </button>
      </div>

      {loading ? <Info text="Loading mission board..." /> : null}
      {error ? <Info text={`Error: ${error}`} danger /> : null}

      <div className="grid gap-4 xl:grid-cols-3">
        {columns.map((column) => (
          <article key={column.id} className="rounded-xl border border-slate-800 bg-slate-900/70 p-3">
            <h3 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">
              {column.title}
            </h3>
            <div className="space-y-2">
              {column.cards.map((card) => (
                <div key={card.id} className="rounded-lg border border-slate-800 bg-slate-950/80 p-3">
                  <p className="text-sm text-slate-100">{card.title}</p>
                  <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-slate-400">
                    {card.owner ? <span>Owner: {card.owner}</span> : null}
                    {card.priority ? <span>Priority: {card.priority}</span> : null}
                  </div>
                </div>
              ))}
            </div>
          </article>
        ))}
      </div>

      {!loading && columns.length === 0 ? (
        <Info text="Mission board is empty or unavailable." />
      ) : null}
    </section>
  )
}

function Info({ text, danger = false }: { text: string; danger?: boolean }) {
  return (
    <div
      className={`rounded-xl border px-4 py-3 text-sm ${
        danger
          ? 'border-rose-500/30 bg-rose-500/10 text-rose-200'
          : 'border-slate-800 bg-slate-900/50 text-slate-300'
      }`}
    >
      {text}
    </div>
  )
}
