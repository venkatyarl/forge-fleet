import { useCallback, useEffect, useMemo, useState } from 'react'
import { getJson } from '../lib/api'

type SoftwareRow = {
  computer_id: string
  computer_name: string
  os_family: string
  software_id: string
  software_display_name: string
  software_kind: string
  installed_version?: string | null
  latest_version?: string | null
  install_source?: string | null
  install_source_identifier?: string | null
  install_path?: string | null
  last_checked_at?: string | null
  status: string
  last_upgrade_error?: string | null
  requires_restart: boolean
  requires_reboot: boolean
  drift: boolean
}

export function SoftwareDriftPanel() {
  const [rows, setRows] = useState<SoftwareRow[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [onlyDrift, setOnlyDrift] = useState(false)

  const load = useCallback(async () => {
    try {
      setError(null)
      const data = await getJson<{ rows: SoftwareRow[] }>('/api/software/computers')
      setRows(data.rows ?? [])
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
    const i = setInterval(() => void load(), 15_000)
    return () => clearInterval(i)
  }, [load])

  // Build the (computer × software) matrix.
  const { computers, software, cellMap, driftCount } = useMemo(() => {
    const computerSet = new Set<string>()
    const softwareMap = new Map<string, string>()
    const cells = new Map<string, SoftwareRow>()
    let drift = 0
    const list = onlyDrift ? rows.filter((r) => r.drift) : rows
    for (const r of list) {
      computerSet.add(r.computer_name)
      softwareMap.set(r.software_id, r.software_display_name)
      cells.set(`${r.computer_name}::${r.software_id}`, r)
      if (r.drift) drift++
    }
    return {
      computers: [...computerSet].sort(),
      software: [...softwareMap.entries()].sort((a, b) => a[1].localeCompare(b[1])),
      cellMap: cells,
      driftCount: drift,
    }
  }, [rows, onlyDrift])

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-semibold text-zinc-100">Software Inventory</h2>
          <p className="text-sm text-zinc-500">
            {rows.length} record{rows.length === 1 ? '' : 's'} · {driftCount} drifted
          </p>
        </div>
        <div className="flex items-center gap-2">
          <label className="flex items-center gap-2 text-xs text-zinc-400">
            <input
              type="checkbox"
              checked={onlyDrift}
              onChange={(e) => setOnlyDrift(e.target.checked)}
              className="accent-violet-500"
            />
            Only drift
          </label>
          <button
            onClick={() => void load()}
            className="rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-1.5 text-sm text-zinc-400 hover:text-zinc-200"
          >
            Refresh
          </button>
        </div>
      </div>

      {error && (
        <div className="rounded-xl border border-rose-500/20 bg-rose-500/5 px-4 py-3 text-sm text-rose-300">
          {error}
        </div>
      )}

      {loading ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : computers.length === 0 ? (
        <div className="rounded-xl border border-zinc-800 bg-zinc-900/40 p-8 text-center text-sm text-zinc-500">
          No software reported yet. Run <span className="font-mono">ff pulse send</span> on each
          computer.
        </div>
      ) : (
        <div className="overflow-auto rounded-xl border border-zinc-800">
          <table className="min-w-full border-collapse text-xs">
            <thead className="bg-zinc-900/80 text-left uppercase tracking-wide text-zinc-500">
              <tr>
                <th className="sticky left-0 z-10 bg-zinc-900/80 px-3 py-2">Software</th>
                {computers.map((c) => (
                  <th key={c} className="px-3 py-2 text-center">
                    {c}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-800">
              {software.map(([sid, sname]) => (
                <tr key={sid} className="hover:bg-zinc-900/40">
                  <td className="sticky left-0 z-10 bg-zinc-950 px-3 py-2 font-medium text-zinc-200">
                    {sname}
                    <div className="text-[10px] font-normal text-zinc-500">{sid}</div>
                  </td>
                  {computers.map((c) => {
                    const cell = cellMap.get(`${c}::${sid}`)
                    if (!cell) {
                      return (
                        <td
                          key={c}
                          className="px-3 py-2 text-center text-zinc-700"
                        >
                          —
                        </td>
                      )
                    }
                    return (
                      <td key={c} className="px-3 py-2 text-center">
                        <div className="flex flex-col items-center gap-0.5">
                          <span
                            className={`font-mono text-[11px] ${
                              cell.drift ? 'text-rose-300' : 'text-zinc-200'
                            }`}
                          >
                            {cell.installed_version ?? '—'}
                          </span>
                          {cell.install_source && (
                            <span className="text-[9px] text-zinc-500">
                              {cell.install_source}
                            </span>
                          )}
                          {cell.drift && cell.latest_version && (
                            <span className="rounded bg-rose-500/20 px-1 py-0.5 text-[9px] text-rose-300">
                              → {cell.latest_version}
                            </span>
                          )}
                        </div>
                      </td>
                    )
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  )
}
