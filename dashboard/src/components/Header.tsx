import type { WsEvent } from '../types'

type HeaderProps = {
  wsConnected: boolean
  eventCount: number
  lastEvent: WsEvent | null
  darkMode: boolean
  onToggleDarkMode: () => void
}

export function Header({
  wsConnected,
  eventCount,
  lastEvent: _lastEvent,
}: HeaderProps) {
  return (
    <header className="border-b border-slate-800 bg-slate-950/90 px-4 py-2 backdrop-blur">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <div className="flex h-8 w-8 items-center justify-center rounded-lg border border-violet-500/30 bg-violet-500/10">
            <span className="text-lg">⚡</span>
          </div>
          <div>
            <h1 className="text-base font-semibold text-slate-100">ForgeFleet</h1>
          </div>
          <span className="text-xs text-slate-500">v{import.meta.env.VITE_VERSION || '2026.4.7'}</span>
        </div>

        <div className="flex items-center gap-3">
          {/* Search trigger */}
          <button
            onClick={() => document.dispatchEvent(new KeyboardEvent('keydown', { key: 'k', metaKey: true }))}
            className="flex items-center gap-2 rounded-lg border border-slate-700 bg-slate-900 px-3 py-1.5 text-xs text-slate-400 hover:border-slate-600 transition"
          >
            <svg className="h-3.5 w-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor"><path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z" /></svg>
            Search
            <kbd className="rounded border border-slate-700 bg-slate-800 px-1 py-0.5 text-[10px]">⌘K</kbd>
          </button>

          {/* WS status */}
          <span className={`inline-flex items-center gap-1.5 rounded-full px-2 py-1 text-xs font-medium ${
            wsConnected ? 'bg-emerald-500/15 text-emerald-400' : 'bg-rose-500/15 text-rose-400'
          }`}>
            <span className={`h-1.5 w-1.5 rounded-full ${wsConnected ? 'bg-emerald-400 animate-pulse' : 'bg-rose-400'}`} />
            {wsConnected ? 'Live' : 'Offline'}
          </span>

          {/* Event count */}
          <span className="rounded-full bg-slate-800/70 px-2 py-1 text-xs text-slate-400">{eventCount}</span>
        </div>
      </div>
    </header>
  )
}
