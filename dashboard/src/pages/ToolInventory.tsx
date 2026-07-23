import { useEffect, useMemo, useState, type ButtonHTMLAttributes } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { cn } from '../lib/utils'

type LiveTool = {
  tool_name: string
  worker_name: string
  description: string
  health_checked_at: string
  call_count: number
  avg_latency_ms: number | null
  healthy: boolean
}

type Tone = 'ok' | 'warn' | 'crit' | 'info' | 'neutral'

async function fetchTools(): Promise<LiveTool[]> {
  try {
    const res = await fetch('/api/tools')
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`)
    const data = await res.json()
    return Array.isArray(data.tools) ? data.tools : []
  } catch {
    return []
  }
}

export function ToolInventory() {
  const [liveTools, setLiveTools] = useState<LiveTool[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [search, setSearch] = useState('')
  const [selectedCategory, setSelectedCategory] = useState<string | null>(null)

  const load = async () => {
    try {
      setError(null)
      const tools = await fetchTools()
      setLiveTools(tools)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load tool registry')
      setLiveTools([])
    }
  }

  useEffect(() => {
    void load()
  }, [])

  const tools = useMemo<LiveTool[]>(() => liveTools ?? [], [liveTools])
  const isLoading = liveTools === null

  const filtered = useMemo(() => {
    return tools.filter((t) => {
      const matchesSearch =
        !search ||
        t.tool_name.toLowerCase().includes(search.toLowerCase()) ||
        t.description.toLowerCase().includes(search.toLowerCase())
      const matchesCategory = !selectedCategory || t.worker_name === selectedCategory
      return matchesSearch && matchesCategory
    })
  }, [tools, search, selectedCategory])

  const categories = useMemo(() => {
    const cats = new Set<string>()
    tools.forEach((t) => cats.add(t.worker_name))
    return [...cats].sort()
  }, [tools])

  const categoryCounts = useMemo(() => {
    const counts: Record<string, number> = {}
    tools.forEach((t) => {
      counts[t.worker_name] = (counts[t.worker_name] || 0) + 1
    })
    return counts
  }, [tools])

  const healthyCount = useMemo(() => tools.filter((t) => t.healthy).length, [tools])
  const unhealthyCount = Math.max(tools.length - healthyCount, 0)
  const totalCalls = useMemo(() => tools.reduce((total, t) => total + (t.call_count ?? 0), 0), [tools])
  const avgLatency = useMemo(() => {
    const latencies = tools
      .map((t) => t.avg_latency_ms)
      .filter((latency): latency is number => latency != null)
    if (latencies.length === 0) return null
    return Math.round(latencies.reduce((total, latency) => total + latency, 0) / latencies.length)
  }, [tools])

  return (
    <section className="min-h-full space-y-6 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Tool Inventory</h1>
            {isLoading ? <Badge variant="info">loading</Badge> : <Badge variant="ok">live</Badge>}
          </div>
          <p className="mt-1 text-sm text-dim">
            {isLoading
              ? 'Loading live tool registry'
              : `${tools.length} tool${tools.length !== 1 ? 's' : ''} across ${categories.length} node${categories.length !== 1 ? 's' : ''}`}
          </p>
        </div>
        <input
          type="text"
          placeholder="Search tools..."
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          aria-label="Search tools"
          className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-foreground outline-hidden transition placeholder:text-dim focus:border-primary sm:w-72"
        />
      </div>

      {error ? (
        <Card className="border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">
          {error}
        </Card>
      ) : null}

      {isLoading ? (
        <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
          {[1, 2, 3, 4, 5, 6, 7, 8].map((item) => (
            <ToolSkeleton key={item} />
          ))}
        </div>
      ) : (
        <>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <SummaryCard
              label="Healthy Tools"
              value={healthyCount}
              detail={`${unhealthyCount} unhealthy`}
              tone={unhealthyCount > 0 ? 'warn' : 'ok'}
            />
            <SummaryCard
              label="Registered Tools"
              value={tools.length}
              detail="live registry"
              tone="info"
            />
            <SummaryCard
              label="Call Volume"
              value={totalCalls.toLocaleString()}
              detail="total recorded calls"
              tone="neutral"
            />
            <SummaryCard
              label="Latency"
              value={avgLatency == null ? 'n/a' : `${avgLatency}ms`}
              detail="average when reported"
              tone="info"
            />
          </div>

          <Card className="bg-surface">
            <CardHeader className="items-start gap-3">
              <div>
                <CardTitle>Registry Filters</CardTitle>
                <CardDescription>
                  {filtered.length} matching {filtered.length === 1 ? 'tool' : 'tools'}
                  {selectedCategory ? ` on ${selectedCategory}` : ''}
                </CardDescription>
              </div>
              <Badge variant={tools.length === 0 ? 'warn' : 'ok'}>
                {tools.length === 0 ? 'no live registry data' : 'live'}
              </Badge>
            </CardHeader>

            <div className="flex flex-wrap gap-2">
              <FilterButton active={!selectedCategory} onClick={() => setSelectedCategory(null)}>
                All ({tools.length})
              </FilterButton>
              {categories.map((cat) => (
                <FilterButton
                  key={cat}
                  active={selectedCategory === cat}
                  onClick={() => setSelectedCategory(selectedCategory === cat ? null : cat)}
                >
                  {cat} ({categoryCounts[cat] || 0})
                </FilterButton>
              ))}
            </div>
          </Card>

          {tools.length === 0 ? (
            <Card className="flex flex-col items-center justify-center bg-panel px-8 py-12 text-center">
              <CardTitle>No tools registered</CardTitle>
              <CardDescription className="mt-2 max-w-md">
                The live tool registry returned no tools. Agents register tools via{' '}
                <code className="rounded-sm bg-elevated px-1 font-mono text-primary">/api/tools/register</code>.
              </CardDescription>
            </Card>
          ) : filtered.length === 0 ? (
            <Card className="flex flex-col items-center justify-center bg-panel px-8 py-12 text-center">
              <CardTitle>No tools found</CardTitle>
              <CardDescription className="mt-2 max-w-md">
                No tools matching &quot;{search}&quot; {selectedCategory ? `in ${selectedCategory}` : ''}
              </CardDescription>
            </Card>
          ) : (
            <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
              {filtered.map((tool) => (
                <ToolCard key={`${tool.tool_name}-${tool.worker_name}`} tool={tool} />
              ))}
            </div>
          )}
        </>
      )}
    </section>
  )
}

function SummaryCard({
  label,
  value,
  detail,
  tone,
}: {
  label: string
  value: string | number
  detail: string
  tone: Tone
}) {
  return (
    <Card className="bg-panel">
      <CardHeader className="mb-2">
        <CardDescription>{label}</CardDescription>
      </CardHeader>
      <div className={cn('truncate text-2xl font-bold', textToneClass(tone))}>{value}</div>
      <div className="mt-1 text-xs text-dim">{detail}</div>
    </Card>
  )
}

function FilterButton({
  active,
  className,
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { active: boolean }) {
  return (
    <Button
      type="button"
      variant="outline"
      size="sm"
      className={cn(
        'rounded-full border-border bg-panel text-muted hover:border-border-subtle hover:bg-elevated hover:text-foreground',
        active && 'border-primary/40 bg-primary-subtle text-primary hover:bg-primary-subtle hover:text-primary',
        className,
      )}
      {...props}
    />
  )
}

function ToolCard({ tool }: { tool: LiveTool }) {
  const healthStatus = tool.healthy ? 'healthy' : 'offline'
  const tone = tool.healthy ? 'ok' : 'crit'
  const callCount = tool.call_count ?? 0

  return (
    <Card className="h-full bg-panel transition hover:border-border-subtle hover:bg-elevated">
      <CardHeader className="items-start gap-3">
        <div className="min-w-0">
          <CardTitle className="truncate font-mono text-base text-status-info">{tool.tool_name}</CardTitle>
          <CardDescription className="mt-1 truncate">{tool.worker_name}</CardDescription>
        </div>
        <span
          className={cn('mt-1 h-2.5 w-2.5 shrink-0 rounded-full bg-current', textToneClass(tone))}
          title={tool.healthy ? 'Healthy' : 'Unhealthy'}
        />
      </CardHeader>

      <p className="min-h-10 text-sm leading-5 text-muted">{tool.description}</p>

      <div className="mt-4 flex flex-wrap gap-1.5">
        <StatusBadge status={healthStatus}>{healthStatus}</StatusBadge>
        <Badge variant="neutral">{tool.worker_name}</Badge>
      </div>

      <dl className="mt-4 grid grid-cols-2 gap-3 border-t border-border pt-3 text-sm">
        <Field label="Calls" value={callCount.toLocaleString()} tone={callCount > 0 ? 'info' : 'neutral'} />
        <Field
          label="Avg Latency"
          value={tool.avg_latency_ms == null ? 'n/a' : `${Math.round(tool.avg_latency_ms)}ms`}
          tone={tool.avg_latency_ms == null ? 'neutral' : 'info'}
        />
        <div className="col-span-2">
          <dt className="text-xs text-dim">Health Check</dt>
          <dd className="truncate text-sm text-muted">{formatHealthCheck(tool.health_checked_at)}</dd>
        </div>
      </dl>
    </Card>
  )
}

function Field({ label, value, tone }: { label: string; value: string; tone: Tone }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-dim">{label}</dt>
      <dd className={cn('truncate text-sm font-medium', textToneClass(tone))}>{value}</dd>
    </div>
  )
}

function ToolSkeleton() {
  return (
    <Card className="space-y-4 bg-panel">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1 space-y-2">
          <div className="h-4 w-32 animate-pulse rounded-sm bg-elevated" />
          <div className="h-3 w-24 animate-pulse rounded-sm bg-elevated" />
        </div>
        <div className="h-2.5 w-2.5 animate-pulse rounded-full bg-elevated" />
      </div>
      <div className="space-y-2">
        <div className="h-3 w-full animate-pulse rounded-sm bg-elevated" />
        <div className="h-3 w-3/4 animate-pulse rounded-sm bg-elevated" />
      </div>
      <div className="grid grid-cols-2 gap-3 border-t border-border pt-3">
        {[1, 2].map((item) => (
          <div key={item} className="space-y-2">
            <div className="h-3 w-14 animate-pulse rounded-sm bg-elevated" />
            <div className="h-4 w-20 animate-pulse rounded-sm bg-elevated" />
          </div>
        ))}
      </div>
    </Card>
  )
}

function formatHealthCheck(value: string): string {
  if (!value) return 'unreported'
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return value
  return date.toLocaleString()
}

function textToneClass(tone: Tone) {
  if (tone === 'ok') return 'text-status-ok'
  if (tone === 'warn') return 'text-status-warn'
  if (tone === 'crit') return 'text-status-crit'
  if (tone === 'info') return 'text-status-info'
  return 'text-foreground'
}
