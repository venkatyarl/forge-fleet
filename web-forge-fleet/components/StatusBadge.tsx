import type { ReactNode } from 'react'

// Semantic status tokens. Every panel should map its domain-specific
// status strings to one of these tokens so the palette stays coherent
// across the 8 Pulse panels.
export type StatusTone =
  | 'ok' // online, healthy, running, active
  | 'warn' // sdown, degraded, maintenance, syncing, loading, warning
  | 'crit' // odown, offline, failed, stopped, error, critical
  | 'info' // pending, starting, info, idle
  | 'role-leader' // the primary / leader role
  | 'neutral' // unknown, disabled, everything else

const TONES: Record<StatusTone, string> = {
  ok: 'bg-emerald-500/15 text-emerald-300 border-emerald-500/30',
  warn: 'bg-amber-500/15 text-amber-300 border-amber-500/30',
  crit: 'bg-rose-500/15 text-rose-300 border-rose-500/30',
  info: 'bg-sky-500/15 text-sky-300 border-sky-500/30',
  'role-leader': 'bg-violet-500/15 text-violet-300 border-violet-500/30',
  neutral: 'bg-zinc-800 text-zinc-300 border-zinc-700',
}

// Map common raw status strings to a tone. Panels may pass an explicit
// `tone` prop if their mapping differs; otherwise we infer from the label.
export function toneFor(status: string | null | undefined): StatusTone {
  const s = (status ?? '').toLowerCase()
  if (!s) return 'neutral'
  switch (s) {
    case 'online':
    case 'healthy':
    case 'ok':
    case 'running':
    case 'active':
    case 'fresh':
    case 'on':
      return 'ok'
    case 'sdown':
    case 'maintenance':
    case 'degraded':
    case 'syncing':
    case 'loading':
    case 'starting':
    case 'restarting':
    case 'paused':
    case 'warning':
    case 'upgrade_available':
    case 'stale':
      return 'warn'
    case 'odown':
    case 'offline':
    case 'failed':
    case 'stopped':
    case 'exited':
    case 'error':
    case 'critical':
    case 'down':
    case 'unhealthy':
    case 'expired':
      return 'crit'
    case 'pending':
    case 'idle':
    case 'info':
      return 'info'
    case 'leader':
    case 'primary':
      return 'role-leader'
    default:
      return 'neutral'
  }
}

export function StatusBadge({
  status,
  tone,
  children,
  title,
}: {
  status?: string | null
  tone?: StatusTone
  children?: ReactNode
  title?: string
}) {
  const resolved = tone ?? toneFor(status)
  return (
    <span
      title={title}
      className={`inline-flex items-center rounded-full border px-2 py-0.5 text-[11px] ${TONES[resolved]}`}
    >
      {children ?? status ?? '—'}
    </span>
  )
}

// Small solid dot with the same tone palette — used in card headers.
export function StatusDot({
  status,
  tone,
}: {
  status?: string | null
  tone?: StatusTone
}) {
  const resolved = tone ?? toneFor(status)
  const bg: Record<StatusTone, string> = {
    ok: 'bg-emerald-400',
    warn: 'bg-amber-400',
    crit: 'bg-rose-500',
    info: 'bg-sky-400',
    'role-leader': 'bg-violet-400',
    neutral: 'bg-zinc-500',
  }
  return <span className={`inline-block h-2.5 w-2.5 rounded-full ${bg[resolved]}`} />
}
