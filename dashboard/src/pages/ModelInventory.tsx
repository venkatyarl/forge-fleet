import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'
import { extractModels } from '../lib/normalizers'
import type { FleetModel, FleetStatusResponse } from '../types'

export function ModelInventory() {
  const [models, setModels] = useState<FleetModel[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const payload = await getJson<FleetStatusResponse>('/api/models').catch(() =>
        getJson<FleetStatusResponse>('/v1/models').catch(() => getJson<FleetStatusResponse>('/api/fleet/status')),
      )
      setModels(extractModels(payload))
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load model inventory')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-xl font-semibold text-slate-100">Model Inventory</h2>
        <button
          onClick={() => void load()}
          className="rounded-md border border-slate-700 bg-slate-900 px-3 py-1.5 text-sm text-slate-200 hover:border-slate-500"
          type="button"
        >
          Refresh
        </button>
      </div>

      {loading ? <Info text="Loading models..." /> : null}
      {error ? <Info text={`Error: ${error}`} danger /> : null}

      <div className="overflow-hidden rounded-xl border border-slate-800 bg-slate-900/70">
        <table className="min-w-full text-left text-sm">
          <thead className="bg-slate-900 text-slate-400">
            <tr>
              <th className="px-3 py-2">Model</th>
              <th className="px-3 py-2">Tier</th>
              <th className="px-3 py-2">Status</th>
              <th className="px-3 py-2">Node</th>
              <th className="px-3 py-2">Context</th>
            </tr>
          </thead>
          <tbody>
            {models.map((model, idx) => (
              <tr key={`${model.id ?? model.name}-${idx}`} className="border-t border-slate-800 text-slate-200">
                <td className="px-3 py-2">{model.name}</td>
                <td className="px-3 py-2">{model.tier ?? '-'}</td>
                <td className="px-3 py-2">{model.status ?? 'unknown'}</td>
                <td className="px-3 py-2">{model.node ?? '-'}</td>
                <td className="px-3 py-2">{model.contextWindow ?? '-'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {!loading && models.length === 0 ? <Info text="No models reported by API yet." /> : null}
    </section>
  )
}

function Info({ text, danger = false }: { text: string; danger?: boolean }) {
  return (
    <div
      className={`rounded-xl border px-4 py-3 text-sm ${
        danger
          ? 'border-rose-500/30 bg-rose-500/10 text-rose-200'
          : 'border-slate-800 bg-slate-900/50 text-slate-300'
      }`}
    >
      {text}
    </div>
  )
}
