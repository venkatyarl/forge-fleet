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
  lastEvent,
  darkMode,
  onToggleDarkMode,
}: HeaderProps) {
  return (
    <header className="border-b border-slate-800 bg-slate-950/90 px-4 py-3 backdrop-blur md:px-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <div className="flex items-center gap-3">
            <img
              src="/brand/forgefleet-mark.svg"
              alt="ForgeFleet"
              className="h-10 w-10 rounded-lg border border-slate-700/80 bg-slate-900/70 p-1"
            />
            <div>
              <h1 className="text-lg font-semibold text-slate-100 md:text-xl">ForgeFleet Dashboard</h1>
              <p className="text-xs font-medium uppercase tracking-[0.18em] text-sky-300/90">
                Command Mesh
              </p>
            </div>
          </div>
          <p className="mt-1 text-xs text-slate-400 md:text-sm">
            Live fleet telemetry, mission control, and LLM routing insights
          </p>
        </div>

        <div className="flex items-center gap-2 text-xs md:text-sm">
          <span
            className={`inline-flex items-center rounded-full px-2 py-1 font-medium ${
              wsConnected
                ? 'bg-emerald-500/20 text-emerald-300'
                : 'bg-rose-500/20 text-rose-300'
            }`}
          >
            {wsConnected ? 'WS connected' : 'WS offline'}
          </span>
          <span className="rounded-full bg-slate-800 px-2 py-1 text-slate-200">Events: {eventCount}</span>
          <button
            onClick={onToggleDarkMode}
            className="rounded-md border border-slate-700 bg-slate-900 px-3 py-1.5 text-slate-200 transition hover:border-slate-500"
            type="button"
          >
            {darkMode ? 'Dark' : 'Light'}
          </button>
        </div>
      </div>

      {lastEvent ? (
        <p className="mt-2 truncate text-xs text-slate-400">
          Last event: <span className="text-slate-300">{lastEvent.type}</span> at{' '}
          {new Date(lastEvent.timestamp).toLocaleTimeString()}
        </p>
      ) : null}
    </header>
  )
}
