import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'
import { PanelHeader, RefreshButton } from './PanelHeader'
import { StatusBadge } from './StatusBadge'

type Container = {
  id: string
  computer_id: string
  computer_name: string
  primary_ip: string
  project_name?: string | null
  compose_file?: string | null
  container_name: string
  container_id?: string | null
  image?: string | null
  ports: Array<string | { host?: number; container?: number; protocol?: string }>
  status: string
  health?: string | null
  last_status_change?: string | null
  first_seen_at?: string | null
  last_seen_at?: string | null
}

type DockerProject = {
  project_name: string
  container_count: number
  running_count: number
  containers: Container[]
}

function healthColor(h?: string | null): string {
  if (!h || h === 'none') return 'text-zinc-500'
  if (h === 'healthy') return 'text-emerald-400'
  if (h === 'starting') return 'text-amber-400'
  return 'text-rose-400'
}

function formatPort(p: unknown): string {
  if (typeof p === 'string') return p
  if (p && typeof p === 'object') {
    const obj = p as { host?: number; container?: number; protocol?: string }
    if (obj.host != null && obj.container != null) {
      return `${obj.host}:${obj.container}${obj.protocol ? `/${obj.protocol}` : ''}`
    }
    if (obj.container != null) return `${obj.container}${obj.protocol ? `/${obj.protocol}` : ''}`
  }
  return String(p)
}

export function DockerPanel() {
  const [projects, setProjects] = useState<DockerProject[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const load = useCallback(async () => {
    try {
      setError(null)
      const data = await getJson<{ projects: DockerProject[] }>('/api/docker/projects')
      setProjects(data.projects ?? [])
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

  const totalContainers = projects.reduce((n, p) => n + p.container_count, 0)
  const runningTotal = projects.reduce((n, p) => n + p.running_count, 0)

  return (
    <section className="space-y-4">
      <PanelHeader
        title="Docker Containers"
        subtitle={`${projects.length} project${projects.length === 1 ? '' : 's'} · ${runningTotal}/${totalContainers} running`}
        rightSlot={<RefreshButton onClick={() => void load()} />}
      />

      {error && (
        <div className="rounded-xl border border-rose-500/20 bg-rose-500/5 px-4 py-3 text-sm text-rose-300">
          {error}
        </div>
      )}

      {loading ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : projects.length === 0 ? (
        <div className="rounded-xl border border-zinc-800 bg-zinc-900/40 p-8 text-center text-sm text-zinc-500">
          No docker containers reported. Pulse v2 reports containers every 60s.
        </div>
      ) : (
        <div className="space-y-4">
          {projects.map((p) => (
            <details
              key={p.project_name}
              open
              className="rounded-xl border border-zinc-800 bg-zinc-900/50"
            >
              <summary className="flex cursor-pointer items-center justify-between px-4 py-3 hover:bg-zinc-900/80">
                <div className="flex items-center gap-2">
                  <span className="text-base font-semibold text-zinc-100">
                    {p.project_name}
                  </span>
                  <span className="rounded bg-zinc-800 px-2 py-0.5 text-[11px] text-zinc-400">
                    {p.running_count}/{p.container_count} up
                  </span>
                </div>
              </summary>
              <div className="overflow-hidden border-t border-zinc-800">
                <table className="w-full text-sm">
                  <thead className="bg-zinc-900/80 text-left text-xs uppercase tracking-wider text-zinc-500">
                    <tr>
                      <th className="px-3 py-2">Container</th>
                      <th className="px-3 py-2">Computer</th>
                      <th className="px-3 py-2">Image</th>
                      <th className="px-3 py-2">Ports</th>
                      <th className="px-3 py-2">Status</th>
                      <th className="px-3 py-2">Health</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y divide-zinc-800">
                    {p.containers.map((c) => (
                      <tr key={c.id} className="hover:bg-zinc-900/40">
                        <td className="px-3 py-2 text-zinc-100">
                          {c.container_name}
                          {c.container_id && (
                            <div className="font-mono text-[10px] text-zinc-500">
                              {c.container_id.slice(0, 12)}
                            </div>
                          )}
                        </td>
                        <td className="px-3 py-2 text-zinc-400">{c.computer_name}</td>
                        <td className="px-3 py-2 font-mono text-xs text-zinc-400">
                          {c.image ?? '—'}
                        </td>
                        <td className="px-3 py-2 font-mono text-xs text-zinc-400">
                          {(c.ports ?? []).map(formatPort).join(', ') || '—'}
                        </td>
                        <td className="px-3 py-2">
                          <StatusBadge status={c.status} />
                        </td>
                        <td className={`px-3 py-2 text-xs ${healthColor(c.health)}`}>
                          {c.health ?? '—'}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </details>
          ))}
        </div>
      )}
    </section>
  )
}
