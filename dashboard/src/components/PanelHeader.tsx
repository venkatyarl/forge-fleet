import type { ReactNode } from 'react'

// Standard header used by every Pulse panel. Use `rightSlot` for
// panel-specific controls (refresh buttons, filters, toggles).
export function PanelHeader({
  title,
  subtitle,
  rightSlot,
}: {
  title: string
  subtitle?: ReactNode
  rightSlot?: ReactNode
}) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-2">
      <div>
        <h2 className="text-xl font-semibold text-zinc-100">{title}</h2>
        {subtitle != null && (
          <p className="text-sm text-zinc-500">{subtitle}</p>
        )}
      </div>
      {rightSlot && <div className="flex items-center gap-2">{rightSlot}</div>}
    </div>
  )
}

// Standard refresh button — paired with PanelHeader in its rightSlot.
export function RefreshButton({
  onClick,
  loading,
}: {
  onClick: () => void
  loading?: boolean
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={loading}
      className="rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-1.5 text-sm text-zinc-400 transition hover:text-zinc-200 disabled:cursor-not-allowed disabled:opacity-60"
    >
      {loading ? 'Refreshing…' : 'Refresh'}
    </button>
  )
}

// Small live indicator — green dot + "live" when SSE is connected,
// zinc dot + "polling" when falling back to interval polling.
export function LiveIndicator({ live }: { live: boolean }) {
  return (
    <span
      title={live ? 'Streaming real-time events via SSE' : 'Polling every 10s'}
      className="inline-flex items-center gap-1.5 rounded-full border border-zinc-800 bg-zinc-900 px-2 py-0.5 text-[11px] text-zinc-400"
    >
      <span
        className={`inline-block h-1.5 w-1.5 rounded-full ${
          live ? 'bg-emerald-400 shadow-[0_0_6px_rgba(52,211,153,0.7)]' : 'bg-zinc-500'
        }`}
      />
      {live ? 'live' : 'polling'}
    </span>
  )
}
