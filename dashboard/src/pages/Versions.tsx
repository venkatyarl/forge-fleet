import { useEffect, useState } from 'react'
import { getJson } from '../lib/api'

interface ToolEntry {
  current?: string
  latest?: string | null
  checked_at?: string
}

interface NodeRow {
  name: string
  tooling?: Record<string, ToolEntry>
}

interface FleetStatus {
  nodes?: NodeRow[]
}

function statusSymbol(entry: ToolEntry | undefined): { icon: string; color: string } {
  if (!entry || !entry.current) return { icon: '—', color: 'text-slate-600' }
  if (!entry.latest) return { icon: '?', color: 'text-slate-400' }
  if (entry.latest === entry.current) return { icon: '✓', color: 'text-emerald-400' }
  return { icon: '⚠', color: 'text-amber-400' }
}

export function Versions() {
  const [nodes, setNodes] = useState<NodeRow[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [selected, setSelected] = useState<{ node: string; tool: string } | null>(null)

  useEffect(() => {
    let cancelled = false
    async function load() {
      try {
        const data = await getJson<FleetStatus>('/api/fleet/status')
        if (cancelled) return
        setNodes(data.nodes || [])
        setLoading(false)
      } catch (e) {
        if (cancelled) return
        setError(e instanceof Error ? e.message : String(e))
        setLoading(false)
      }
    }
    load()
    const timer = setInterval(load, 30000)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [])

  const allTools = new Set<string>()
  for (const n of nodes) {
    if (n.tooling) {
      for (const k of Object.keys(n.tooling)) allTools.add(k)
    }
  }
  const toolKeys = Array.from(allTools).sort()

  if (loading) {
    return <div className="p-6 text-slate-400">Loading version matrix…</div>
  }
  if (error) {
    return <div className="p-6 text-rose-400">Error: {error}</div>
  }
  if (toolKeys.length === 0) {
    return (
      <div className="p-6 text-slate-400">
        <div>No tool-version data yet.</div>
        <div className="text-sm mt-2">
          Run <code className="bg-slate-800 px-1 rounded">ff daemon</code> on each node for ~6 h
          OR trigger a manual <code className="bg-slate-800 px-1 rounded">version_check</code>{' '}
          pass.
        </div>
      </div>
    )
  }

  const selEntry =
    selected && nodes.find((n) => n.name === selected.node)?.tooling?.[selected.tool]

  return (
    <div className="p-6 text-slate-100">
      <h2 className="text-xl font-semibold mb-1">Fleet versions — drift monitor</h2>
      <p className="text-sm text-slate-400 mb-4">
        ✓ = current == latest &nbsp;&nbsp;&nbsp; ⚠ = out of date &nbsp;&nbsp;&nbsp; ? = no upstream
        info &nbsp;&nbsp;&nbsp; — = not applicable
      </p>

      <div className="overflow-x-auto">
        <table className="border-collapse text-sm">
          <thead>
            <tr>
              <th className="text-left px-3 py-2 border-b border-slate-700">Tool</th>
              {nodes.map((n) => (
                <th key={n.name} className="text-left px-3 py-2 border-b border-slate-700">
                  {n.name}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {toolKeys.map((tool) => (
              <tr key={tool} className="hover:bg-slate-900/50">
                <td className="px-3 py-1.5 font-mono text-slate-300 border-b border-slate-800">
                  {tool}
                </td>
                {nodes.map((n) => {
                  const e = n.tooling?.[tool]
                  const sym = statusSymbol(e)
                  const cur = e?.current || '—'
                  return (
                    <td
                      key={n.name}
                      onClick={() => setSelected({ node: n.name, tool })}
                      className="px-3 py-1.5 border-b border-slate-800 cursor-pointer font-mono text-xs"
                    >
                      <span className={sym.color}>{sym.icon}</span>{' '}
                      <span className="text-slate-300">{cur.slice(0, 20)}</span>
                    </td>
                  )
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {selected && selEntry && (
        <div className="mt-6 border border-slate-700 rounded p-4 bg-slate-900/60">
          <div className="text-[11px] uppercase tracking-wider text-slate-400 mb-1">
            {selected.node} / {selected.tool}
          </div>
          <div className="grid grid-cols-2 gap-x-6 gap-y-1 text-sm font-mono">
            <div className="text-slate-400">current</div>
            <div className="text-emerald-200">{selEntry.current || '—'}</div>
            <div className="text-slate-400">latest</div>
            <div className="text-amber-200">{selEntry.latest || '(unknown)'}</div>
            <div className="text-slate-400">checked_at</div>
            <div className="text-slate-300">{selEntry.checked_at || '—'}</div>
          </div>
          <button
            onClick={() => setSelected(null)}
            className="mt-3 text-xs px-2 py-1 border border-slate-700 rounded text-slate-300 hover:bg-slate-800"
          >
            Close
          </button>
        </div>
      )}
    </div>
  )
}
