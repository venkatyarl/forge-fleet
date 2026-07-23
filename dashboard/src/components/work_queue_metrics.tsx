import { useMemo } from 'react'
import { ListTodo } from 'lucide-react'
import { Card, CardDescription, CardHeader, CardTitle } from './ui/card'
import { Badge } from './ui/badge'
import { useWorkItems } from '../features/hooks/useDashboardQueries'
import type { WorkItem } from '../protocol/types'

const PRIORITY_ORDER = [1, 2, 3, 4, 5] as const

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

function priorityVariant(priority: number): 'crit' | 'warn' | 'info' | 'neutral' {
  if (priority <= 1) return 'crit'
  if (priority === 2) return 'warn'
  if (priority === 3) return 'info'
  return 'neutral'
}

function computeMetrics(items: WorkItem[] | undefined) {
  const workItems = items ?? []
  const queueSize = workItems.filter((item) => item.status !== 'done').length
  const distribution = new Map<number, number>()

  for (const item of workItems) {
    if (item.status === 'done') continue
    const priority = typeof item.priority === 'number' ? item.priority : 3
    distribution.set(priority, (distribution.get(priority) ?? 0) + 1)
  }

  return { queueSize, distribution }
}

export function WorkQueueMetrics() {
  const { data: workItems = [], isLoading } = useWorkItems()
  const { queueSize, distribution } = useMemo(() => computeMetrics(workItems), [workItems])

  return (
    <Card>
      <CardHeader>
        <div>
          <CardTitle className="flex items-center gap-2">
            <ListTodo className="h-4 w-4" />
            Work Queue Metrics
          </CardTitle>
          <CardDescription>Open items and priority distribution</CardDescription>
        </div>
        <Badge variant="info">{isLoading ? '…' : queueSize} queued</Badge>
      </CardHeader>

      {isLoading ? (
        <div className="space-y-2">
          <div className="h-4 w-full animate-pulse rounded-sm bg-elevated" />
          <div className="h-4 w-3/4 animate-pulse rounded-sm bg-elevated" />
        </div>
      ) : queueSize === 0 ? (
        <p className="text-sm text-dim">No open work items</p>
      ) : (
        <div className="space-y-3">
          <div className="flex items-center justify-between text-sm">
            <span className="text-muted">Queue size</span>
            <span className="font-semibold text-foreground">{queueSize}</span>
          </div>
          <div className="space-y-2">
            <span className="text-xs text-dim">Priority distribution</span>
            <div className="flex flex-wrap gap-2">
              {PRIORITY_ORDER.map((priority) => {
                const count = distribution.get(priority) ?? 0
                return (
                  <Badge
                    key={priority}
                    variant={priorityVariant(priority)}
                    className={count === 0 ? 'opacity-50' : undefined}
                  >
                    {priorityLabel(priority)}: {count}
                  </Badge>
                )
              })}
            </div>
          </div>
        </div>
      )}
    </Card>
  )
}
