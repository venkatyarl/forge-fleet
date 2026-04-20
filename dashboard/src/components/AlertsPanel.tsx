import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'
import { useFleetEvents } from '../lib/useFleetEvents'
import { LiveIndicator, PanelHeader, RefreshButton } from './PanelHeader'
import { StatusBadge, toneFor } from './StatusBadge'

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

// Severity → StatusBadge tone. Keeps the alerts palette aligned with
// the rest of the dashboard (warn = amber, crit = rose, info = sky).
function sevTone(sev: string) {
  switch (sev) {
    case 'critical':
      return toneFor('critical')
    case 'warning':
      return toneFor('warning')
    case 'info':
      return toneFor('info')
    default:
      return toneFor(sev)
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

  // Instant refresh on any fleet event that could affect alert state.
  const { live } = useFleetEvents((evt) => {
    if (
      evt.subject.startsWith('fleet.events.alert.') ||
      evt.subject.startsWith('fleet.events.member.')
    ) {
      void load()
    }
  })

  return (
    <section className="space-y-6">
      <PanelHeader
        title="Alerts"
        subtitle={`${events.length} active · ${policies.length} policies`}
        rightSlot={
          <>
            <LiveIndicator live={live} />
            <RefreshButton onClick={() => void load()} />
          </>
        }
      />

      {error && (
        <div className="rounded-xl border border-rose-500/20 bg-rose-500/5 px-4 py-3 text-sm text-rose-300">
          {error}
        </div>
      )}

      {/* Active events */}
      <div>
        <h3 className="mb-2 text-xs uppercase tracking-wider text-zinc-500">
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
                className="flex flex-wrap items-start justify-between gap-3 rounded-xl border border-zinc-800 bg-zinc-900/50 p-3"
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <StatusBadge tone={sevTone(e.severity)} status={e.severity} />
                    <span className="text-sm font-medium text-zinc-100">
                      {e.policy_name}
                    </span>
                    {e.computer_name && (
                      <span className="text-xs text-zinc-400">@ {e.computer_name}</span>
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
        <h3 className="mb-2 text-xs uppercase tracking-wider text-zinc-500">
          Policies
        </h3>
        <div className="overflow-hidden rounded-xl border border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-900/80 text-left text-xs uppercase tracking-wider text-zinc-500">
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
                    <StatusBadge tone={sevTone(p.severity)} status={p.severity} />
                  </td>
                  <td className="px-3 py-2 text-zinc-400">{p.channel}</td>
                  <td className="px-3 py-2 text-zinc-400">{p.duration_secs}s</td>
                  <td className="px-3 py-2">
                    {p.enabled ? (
                      <span className="text-emerald-400">on</span>
                    ) : (
                      <span className="text-zinc-500">off</span>
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
