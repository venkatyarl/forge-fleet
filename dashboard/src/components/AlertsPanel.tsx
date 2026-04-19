import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'

type AlertEvent = {
  id: string
  policy_id: string
  policy_name: string
  severity: string
  metric: string
  computer_id?: string | null
  computer_name?: string | null
  fired_at: string
  resolved_at?: string | null
  value?: number | null
  value_text?: string | null
  message?: string | null
  channel_result?: string | null
}

type Policy = {
  id: string
  name: string
  description?: string | null
  metric: string
  scope: string
  condition: string
  duration_secs: number
  severity: string
  cooldown_secs: number
  channel: string
  enabled: boolean
}

function sevBadge(sev: string): string {
  switch (sev) {
    case 'critical':
      return 'bg-rose-500/15 text-rose-300 border-rose-500/30'
    case 'warning':
      return 'bg-amber-500/15 text-amber-300 border-amber-500/30'
    case 'info':
      return 'bg-sky-500/15 text-sky-300 border-sky-500/30'
    default:
      return 'bg-zinc-800 text-zinc-400 border-zinc-700'
  }
}

export function AlertsPanel() {
  const [events, setEvents] = useState<AlertEvent[]>([])
  const [policies, setPolicies] = useState<Policy[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const load = useCallback(async () => {
    try {
      setError(null)
      const [ev, pol] = await Promise.all([
        getJson<{ events: AlertEvent[] }>('/api/alerts/events?active=true'),
        getJson<{ policies: Policy[] }>('/api/alerts/policies'),
      ])
      setEvents(ev.events ?? [])
      setPolicies(pol.policies ?? [])
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
    const i = setInterval(() => void load(), 10_000)
    return () => clearInterval(i)
  }, [load])

  return (
    <section className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-semibold text-zinc-100">Alerts</h2>
          <p className="text-sm text-zinc-500">
            {events.length} active · {policies.length} policies
          </p>
        </div>
        <button
          onClick={() => void load()}
          className="rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-1.5 text-sm text-zinc-400 hover:text-zinc-200"
        >
          Refresh
        </button>
      </div>

      {error && (
        <div className="rounded-xl border border-rose-500/20 bg-rose-500/5 px-4 py-3 text-sm text-rose-300">
          {error}
        </div>
      )}

      {/* Active events */}
      <div>
        <h3 className="mb-2 text-sm font-semibold uppercase tracking-wide text-zinc-400">
          Active Events
        </h3>
        {events.length === 0 && !loading ? (
          <div className="rounded-xl border border-emerald-500/20 bg-emerald-500/5 px-4 py-3 text-sm text-emerald-300">
            All clear — no unresolved alerts.
          </div>
        ) : (
          <div className="space-y-2">
            {events.map((e) => (
              <article
                key={e.id}
                className="flex flex-wrap items-start justify-between gap-3 rounded-xl border border-zinc-800 bg-zinc-900/70 p-3"
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span
                      className={`rounded-full border px-2 py-0.5 text-[11px] ${sevBadge(e.severity)}`}
                    >
                      {e.severity}
                    </span>
                    <span className="text-sm font-medium text-zinc-100">
                      {e.policy_name}
                    </span>
                    {e.computer_name && (
                      <span className="text-xs text-zinc-500">@ {e.computer_name}</span>
                    )}
                  </div>
                  {e.message && (
                    <p className="mt-1 text-sm text-zinc-300">{e.message}</p>
                  )}
                  <p className="mt-1 text-[11px] text-zinc-500">
                    {e.metric}
                    {e.value != null ? ` = ${e.value.toFixed(2)}` : ''}
                    {e.value_text ? ` (${e.value_text})` : ''}
                    {' · '}fired {new Date(e.fired_at).toLocaleString()}
                  </p>
                </div>
                {e.channel_result && (
                  <span className="rounded bg-zinc-800 px-2 py-0.5 text-[10px] text-zinc-400">
                    {e.channel_result}
                  </span>
                )}
              </article>
            ))}
          </div>
        )}
      </div>

      {/* Policies */}
      <div>
        <h3 className="mb-2 text-sm font-semibold uppercase tracking-wide text-zinc-400">
          Policies
        </h3>
        <div className="overflow-hidden rounded-xl border border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-900/80 text-left text-xs uppercase tracking-wide text-zinc-500">
              <tr>
                <th className="px-3 py-2">Name</th>
                <th className="px-3 py-2">Metric</th>
                <th className="px-3 py-2">Condition</th>
                <th className="px-3 py-2">Severity</th>
                <th className="px-3 py-2">Channel</th>
                <th className="px-3 py-2">Dur</th>
                <th className="px-3 py-2">Enabled</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-800">
              {policies.map((p) => (
                <tr key={p.id} className="hover:bg-zinc-900/40">
                  <td className="px-3 py-2 text-zinc-100">
                    {p.name}
                    {p.description && (
                      <div className="text-[11px] text-zinc-500">{p.description}</div>
                    )}
                  </td>
                  <td className="px-3 py-2 font-mono text-xs text-zinc-300">{p.metric}</td>
                  <td className="px-3 py-2 font-mono text-xs text-zinc-300">
                    {p.condition}
                  </td>
                  <td className="px-3 py-2">
                    <span
                      className={`rounded-full border px-2 py-0.5 text-[11px] ${sevBadge(p.severity)}`}
                    >
                      {p.severity}
                    </span>
                  </td>
                  <td className="px-3 py-2 text-zinc-400">{p.channel}</td>
                  <td className="px-3 py-2 text-zinc-400">{p.duration_secs}s</td>
                  <td className="px-3 py-2">
                    {p.enabled ? (
                      <span className="text-emerald-400">on</span>
                    ) : (
                      <span className="text-zinc-600">off</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {!loading && policies.length === 0 && (
            <div className="p-4 text-center text-sm text-zinc-500">
              No alert policies defined.
            </div>
          )}
        </div>
      </div>
    </section>
  )
}
