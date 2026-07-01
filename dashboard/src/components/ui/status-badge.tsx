import { Badge } from './badge'

export type StatusTone = 'ok' | 'warn' | 'crit' | 'info' | 'neutral'

const toneFor = (status: string): StatusTone => {
  const s = status.toLowerCase()
  if (['online', 'healthy', 'ready', 'done', 'success', 'passed', 'active'].includes(s)) return 'ok'
  if (['offline', 'failed', 'error', 'critical', 'down'].includes(s)) return 'crit'
  if (['busy', 'thinking', 'running', 'building', 'in_review', 'warning', 'degraded'].includes(s)) return 'warn'
  if (['info', 'pending', 'queued', 'standby'].includes(s)) return 'info'
  return 'neutral'
}

interface StatusBadgeProps {
  status: string
  children?: React.ReactNode
}

export function StatusBadge({ status, children }: StatusBadgeProps) {
  return <Badge variant={toneFor(status)}>{children ?? status}</Badge>
}
