import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'
import { PanelHeader, RefreshButton } from './PanelHeader'
import { StatusBadge } from './StatusBadge'

type LlmServer = {
  id: string
  computer_id: string
  computer_name: string
  primary_ip: string
  model_id: string
  model_display_name?: string | null
  model_family?: string | null
  runtime: string
  endpoint: string
  openai_compatible: boolean
  context_window?: number | null
  parallel_slots?: number | null
  pid?: number | null
  status: string
  healthy: boolean
  ram_allocated_gb?: number | null
  vram_allocated_gb?: number | null
  started_at?: string | null
  queue_depth?: number | null
  tokens_per_sec?: number | null
}

export function LlmTopologyPanel() {
  const [rows, setRows] = useState<LlmServer[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const load = useCallback(async () => {
    try {
      setError(null)
      const data = await getJson<{ servers: LlmServer[] }>('/api/llm/servers')
      setRows(data.servers ?? [])
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
    <section className="space-y-4">
      <PanelHeader
        title="LLM Topology"
        subtitle={`${rows.length} active deployment${rows.length === 1 ? '' : 's'} fleet-wide`}
        rightSlot={<RefreshButton onClick={() => void load()} />}
      />

      {error && (
        <div className="rounded-xl border border-rose-500/20 bg-rose-500/5 px-4 py-3 text-sm text-rose-300">
          {error}
        </div>
      )}

      {loading ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : (
        <div className="overflow-hidden rounded-xl border border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-900/80 text-left text-xs uppercase tracking-wider text-zinc-500">
              <tr>
                <th className="px-3 py-2">Computer</th>
                <th className="px-3 py-2">Model</th>
                <th className="px-3 py-2">Runtime</th>
                <th className="px-3 py-2">Endpoint</th>
                <th className="px-3 py-2">Ctx</th>
                <th className="px-3 py-2">Slots</th>
                <th className="px-3 py-2">Queue</th>
                <th className="px-3 py-2">Tok/s</th>
                <th className="px-3 py-2">Status</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-800">
              {rows.map((s) => (
                <tr key={s.id} className="hover:bg-zinc-900/40">
                  <td className="px-3 py-2 text-zinc-100">
                    <div>{s.computer_name}</div>
                    <div className="text-[11px] text-zinc-500">{s.primary_ip}</div>
                  </td>
                  <td className="px-3 py-2 text-zinc-200">
                    <div>{s.model_display_name ?? s.model_id}</div>
                    <div className="text-[11px] text-zinc-500">
                      {s.model_family ?? ''}
                    </div>
                  </td>
                  <td className="px-3 py-2 text-zinc-300">{s.runtime}</td>
                  <td className="px-3 py-2 font-mono text-xs text-zinc-400">
                    {s.endpoint}
                  </td>
                  <td className="px-3 py-2 text-zinc-400">
                    {s.context_window ?? '—'}
                  </td>
                  <td className="px-3 py-2 text-zinc-400">
                    {s.parallel_slots ?? '—'}
                  </td>
                  <td className="px-3 py-2 text-zinc-400">
                    {s.queue_depth ?? '—'}
                  </td>
                  <td className="px-3 py-2 text-zinc-400">
                    {s.tokens_per_sec == null ? '—' : s.tokens_per_sec.toFixed(1)}
                  </td>
                  <td className="px-3 py-2">
                    <StatusBadge status={s.status} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {rows.length === 0 && (
            <div className="p-8 text-center text-sm text-zinc-500">
              No active LLM deployments. Use <span className="font-mono">ff model load</span> to
              start one.
            </div>
          )}
        </div>
      )}
    </section>
  )
}
