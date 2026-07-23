import { Search } from 'lucide-react'
import type { DashboardEvent } from '../protocol/types'
import { useUIStore } from '../app/store'

type HeaderProps = {
  wsConnected: boolean
  eventCount: number
  lastEvent: DashboardEvent | null
  darkMode: boolean
  onToggleDarkMode: () => void
}

export function Header({ wsConnected, eventCount }: HeaderProps) {
  const setPaletteOpen = useUIStore((s) => s.setCommandPaletteOpen)

  return (
    <header className="border-b border-border bg-background/95 px-4 py-2 backdrop-blur-sm">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <div className="flex h-8 w-8 items-center justify-center rounded-lg border border-primary/30 bg-primary-subtle">
            <span className="text-lg">⚡</span>
          </div>
          <div>
            <h1 className="text-base font-semibold text-foreground">ForgeFleet</h1>
          </div>
          <span className="text-xs text-dim">v{import.meta.env.VITE_VERSION || '2026.4.7'}</span>
        </div>

        <div className="flex items-center gap-3">
          <button
            onClick={() => setPaletteOpen(true)}
            className="flex items-center gap-2 rounded-lg border border-border-subtle bg-panel px-3 py-1.5 text-xs text-muted transition hover:border-border"
          >
            <Search className="h-3.5 w-3.5" />
            Search
            <kbd className="rounded-sm border border-border-subtle bg-elevated px-1 py-0.5 text-[10px]">⌘K</kbd>
          </button>

          <span
            className={`inline-flex items-center gap-1.5 rounded-full px-2 py-1 text-xs font-medium ${
              wsConnected
                ? 'bg-emerald-500/10 text-status-ok'
                : 'bg-rose-500/10 text-status-crit'
            }`}
          >
            <span
              className={`h-1.5 w-1.5 rounded-full ${
                wsConnected ? 'animate-pulse bg-status-ok' : 'bg-status-crit'
              }`}
            />
            {wsConnected ? 'Live' : 'Offline'}
          </span>

          <span className="rounded-full bg-panel px-2 py-1 text-xs text-muted">{eventCount}</span>
        </div>
      </div>
    </header>
  )
}
