import { useEffect, useMemo, useState } from 'react'
import { Badge, type BadgeProps } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { getJson } from '../lib/api'
import { cn } from '../lib/utils'

interface ToolEntry {
  current?: string
  latest?: string | null
  checked_at?: string
}

interface NodeRow {
  name: string
  tooling?: Record<string, ToolEntry>
}

interface FleetTooling {
  nodes?: NodeRow[]
}

interface DeferredTask {
  id: string
  title: string
  kind: string
  status: string
  preferred_node: string | null
  payload: { tool?: string; current?: string; latest?: string }
  attempts: number
  max_attempts: number
  last_error?: string | null
}

function versionStatus(entry: ToolEntry | undefined): {
  label: string
  status: string
  badge: BadgeProps['variant']
  textClass: string
} {
  if (!entry || !entry.current) {
    return { label: 'n/a', status: 'unknown', badge: 'neutral', textClass: 'text-dim' }
  }
  if (!entry.latest) {
    return { label: 'unknown', status: 'info', badge: 'info', textClass: 'text-status-info' }
  }
  if (entry.latest === entry.current) {
    return { label: 'current', status: 'active', badge: 'ok', textClass: 'text-status-ok' }
  }
  return { label: 'upgrade', status: 'warning', badge: 'warn', textClass: 'text-status-warn' }
}

export function Versions() {
  const [nodes, setNodes] = useState<NodeRow[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [selected, setSelected] = useState<{ node: string; tool: string } | null>(null)
  const [upgradeTask, setUpgradeTask] = useState<DeferredTask | null>(null)
  const [applying, setApplying] = useState(false)
  const [applyMsg, setApplyMsg] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    async function load() {
      try {
        const data = await getJson<FleetTooling>('/api/fleet/tooling')
        if (cancelled) return
        setNodes(data.nodes || [])
        setLoading(false)
      } catch (e) {
        if (cancelled) return
        setError(e instanceof Error ? e.message : String(e))
        setLoading(false)
      }
    }
    load()
    const timer = setInterval(load, 30000)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [])

  const toolKeys = useMemo(() => {
    const allTools = new Set<string>()
    for (const n of nodes) {
      if (n.tooling) {
        for (const k of Object.keys(n.tooling)) allTools.add(k)
      }
    }
    return Array.from(allTools).sort()
  }, [nodes])

  const selEntry =
    selected && nodes.find((n) => n.name === selected.node)?.tooling?.[selected.tool]

  useEffect(() => {
    if (!selected) {
      setUpgradeTask(null)
      setApplyMsg(null)
      return
    }
    const drift = selEntry && selEntry.latest && selEntry.current !== selEntry.latest
    if (!drift) {
      setUpgradeTask(null)
      return
    }
    const qs = new URLSearchParams({
      status: 'pending',
      kind: 'upgrade',
      node: selected.node,
      tool: selected.tool,
    })
    getJson<{ tasks: DeferredTask[] }>(`/api/fleet/deferred?${qs.toString()}`)
      .then((r) => setUpgradeTask(r.tasks[0] || null))
      .catch(() => setUpgradeTask(null))
  }, [selected, selEntry])

  const applyUpgrade = async () => {
    if (!upgradeTask) return
    setApplying(true)
    setApplyMsg(null)
    try {
      const r = await fetch(`/api/fleet/deferred/${upgradeTask.id}/promote`, { method: 'POST' })
      const d = await r.json()
      setApplyMsg(d.promoted ? 'Promoted — worker will pick up in next cycle.' : 'Task already dispatched or terminal.')
      setUpgradeTask(null)
    } catch (e) {
      setApplyMsg(e instanceof Error ? e.message : String(e))
    } finally {
      setApplying(false)
    }
  }

  const statusCounts = useMemo(() => {
    const counts = { current: 0, upgrades: 0, unknown: 0, missing: 0 }
    for (const tool of toolKeys) {
      for (const node of nodes) {
        const entry = node.tooling?.[tool]
        if (!entry || !entry.current) counts.missing += 1
        else if (!entry.latest) counts.unknown += 1
        else if (entry.latest === entry.current) counts.current += 1
        else counts.upgrades += 1
      }
    }
    return counts
  }, [nodes, toolKeys])

  if (loading) {
    return (
      <section className="min-h-full space-y-6 bg-background text-foreground">
        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Fleet Versions</CardTitle>
              <CardDescription>Loading version matrix...</CardDescription>
            </div>
            <Badge variant="info">loading</Badge>
          </CardHeader>
          <div className="space-y-3">
            <div className="h-4 w-48 animate-pulse rounded-sm bg-elevated" />
            <div className="h-24 animate-pulse rounded-lg bg-elevated" />
          </div>
        </Card>
      </section>
    )
  }

  if (error) {
    return (
      <section className="min-h-full bg-background text-foreground">
        <Card className="border-border bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Fleet Versions</CardTitle>
              <CardDescription>Could not load tooling data</CardDescription>
            </div>
            <StatusBadge status="error">error</StatusBadge>
          </CardHeader>
          <p className="text-sm text-status-crit">{error}</p>
        </Card>
      </section>
    )
  }

  if (toolKeys.length === 0) {
    return (
      <section className="min-h-full bg-background text-foreground">
        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Fleet Versions</CardTitle>
              <CardDescription>No tool-version data yet.</CardDescription>
            </div>
            <Badge variant="neutral">empty</Badge>
          </CardHeader>
          <p className="text-sm text-muted">
            Run <code className="rounded-sm bg-elevated px-1 text-foreground">ff daemon</code> on
            each node for ~6 h OR trigger a manual{' '}
            <code className="rounded-sm bg-elevated px-1 text-foreground">version_check</code> pass.
          </p>
        </Card>
      </section>
    )
  }

  return (
    <section className="min-h-full space-y-6 bg-background text-foreground">
      <div className="flex flex-col gap-3 md:flex-row md:items-end md:justify-between">
        <div>
          <h2 className="text-xl font-semibold">Fleet Versions</h2>
          <p className="mt-1 text-sm text-dim">Drift monitor for tooling versions across fleet nodes.</p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Badge variant="ok">{statusCounts.current} current</Badge>
          <Badge variant="warn">{statusCounts.upgrades} upgrades</Badge>
          <Badge variant="info">{statusCounts.unknown} unknown</Badge>
          <Badge variant="neutral">{statusCounts.missing} n/a</Badge>
        </div>
      </div>

      <Card className="bg-surface">
        <CardHeader className="items-start gap-3">
          <div>
            <CardTitle>Version Matrix</CardTitle>
            <CardDescription>
              Select a tool/node cell to inspect the current version, upstream version, and upgrade task.
            </CardDescription>
          </div>
          <Badge variant="neutral">
            {toolKeys.length} tools / {nodes.length} nodes
          </Badge>
        </CardHeader>

        <div className="overflow-x-auto rounded-lg border border-border bg-panel">
          <table className="min-w-full border-collapse text-sm">
            <thead className="bg-elevated text-xs uppercase tracking-wide text-dim">
              <tr>
                <th className="whitespace-nowrap border-b border-border px-3 py-2 text-left font-semibold">
                  Tool
                </th>
                {nodes.map((n) => (
                  <th
                    key={n.name}
                    className="whitespace-nowrap border-b border-border px-3 py-2 text-left font-semibold"
                  >
                    {n.name}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {toolKeys.map((tool) => (
                <tr key={tool} className="border-b border-border last:border-0">
                  <td className="sticky left-0 z-10 whitespace-nowrap border-r border-border bg-panel px-3 py-2 font-mono text-xs text-foreground">
                    {tool}
                  </td>
                  {nodes.map((n) => {
                    const entry = n.tooling?.[tool]
                    const status = versionStatus(entry)
                    const current = entry?.current || '—'
                    const isSelected = selected?.node === n.name && selected.tool === tool
                    return (
                      <td
                        key={n.name}
                        onClick={() => setSelected({ node: n.name, tool })}
                        className={cn(
                          'min-w-40 cursor-pointer px-3 py-2 align-top transition hover:bg-elevated',
                          isSelected && 'bg-primary-subtle'
                        )}
                      >
                        <div className="flex items-center justify-between gap-2">
                          <span className={cn('truncate font-mono text-xs', status.textClass)}>
                            {current.slice(0, 20)}
                          </span>
                          <Badge variant={status.badge}>{status.label}</Badge>
                        </div>
                      </td>
                    )
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Card>

      {selected && (
        <Card className="bg-panel">
          <CardHeader className="items-start gap-3">
            <div>
              <CardTitle>{selected.tool}</CardTitle>
              <CardDescription>{selected.node}</CardDescription>
            </div>
            <StatusBadge status={versionStatus(selEntry || undefined).status}>
              {versionStatus(selEntry || undefined).label}
            </StatusBadge>
          </CardHeader>

          {selEntry ? (
            <div className="grid gap-3 text-sm md:grid-cols-3">
              <VersionField label="current" value={selEntry.current || '—'} tone="ok" />
              <VersionField label="latest" value={selEntry.latest || '(unknown)'} tone="warn" />
              <VersionField label="checked_at" value={selEntry.checked_at || '—'} />
            </div>
          ) : (
            <p className="text-sm text-dim">This tool is not reported by the selected node.</p>
          )}

          {upgradeTask ? (
            <div className="mt-4 rounded-lg border border-border bg-surface p-3">
              <div className="mb-2 flex flex-wrap items-center gap-2">
                <StatusBadge status={upgradeTask.status}>{upgradeTask.status}</StatusBadge>
                <Badge variant="warn">upgrade available</Badge>
                {upgradeTask.attempts > 0 && (
                  <Badge variant="neutral">
                    attempts {upgradeTask.attempts}/{upgradeTask.max_attempts}
                  </Badge>
                )}
              </div>
              <p className="mb-3 text-sm text-muted">{upgradeTask.title}</p>
              <Button
                onClick={applyUpgrade}
                disabled={applying}
                size="sm"
              >
                {applying ? 'Promoting...' : `Apply on ${selected.node}`}
              </Button>
              {applyMsg && <div className="mt-2 text-xs text-muted">{applyMsg}</div>}
            </div>
          ) : (
            selEntry &&
            selEntry.latest &&
            selEntry.current !== selEntry.latest && (
              <div className="mt-4 rounded-lg border border-border bg-surface p-3 text-xs text-dim">
                No pending upgrade task; it will be enqueued on the next version_check pass.
              </div>
            )
          )}
          <Button
            onClick={() => setSelected(null)}
            variant="outline"
            size="sm"
            className="mt-4"
          >
            Close
          </Button>
        </Card>
      )}
    </section>
  )
}

function VersionField({
  label,
  value,
  tone,
}: {
  label: string
  value: string
  tone?: 'ok' | 'warn'
}) {
  return (
    <div className="rounded-lg border border-border bg-surface px-3 py-2">
      <div className="text-xs font-medium uppercase tracking-wide text-dim">{label}</div>
      <div
        className={cn(
          'mt-1 break-all font-mono text-sm text-muted',
          tone === 'ok' && 'text-status-ok',
          tone === 'warn' && 'text-status-warn'
        )}
      >
        {value}
      </div>
    </div>
  )
}
