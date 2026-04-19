import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'

type Environment = {
  id: string
  name: string
  deployed_commit_sha?: string | null
  deployed_at?: string | null
  deploy_status?: string | null
  health_status?: string | null
  url?: string | null
}

type Project = {
  id: string
  display_name: string
  compose_file?: string | null
  repo_url?: string | null
  default_branch: string
  main_commit_sha?: string | null
  main_commit_message?: string | null
  main_committed_at?: string | null
  main_committed_by?: string | null
  main_last_synced_at?: string | null
  target_computers: string[]
  health_endpoint?: string | null
  status: string
  active_branch_count: number
  environments: Environment[]
}

function timeAgo(iso?: string | null): string {
  if (!iso) return '—'
  const diff = Date.now() - new Date(iso).getTime()
  const mins = Math.floor(diff / 60_000)
  if (mins < 1) return 'just now'
  if (mins < 60) return `${mins}m ago`
  const hours = Math.floor(mins / 60)
  if (hours < 24) return `${hours}h ago`
  return `${Math.floor(hours / 24)}d ago`
}

function envBadge(status?: string | null): string {
  const v = (status ?? '').toLowerCase()
  if (v === 'healthy' || v === 'ok' || v === 'running')
    return 'bg-emerald-500/15 text-emerald-300 border-emerald-500/30'
  if (v === 'degraded' || v === 'starting')
    return 'bg-amber-500/15 text-amber-300 border-amber-500/30'
  if (v === 'down' || v === 'failed' || v === 'unhealthy')
    return 'bg-rose-500/15 text-rose-300 border-rose-500/30'
  return 'bg-zinc-800 text-zinc-400 border-zinc-700'
}

export function ProjectsPanel() {
  const [projects, setProjects] = useState<Project[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const load = useCallback(async () => {
    try {
      setError(null)
      const data = await getJson<{ projects: Project[] }>('/api/projects')
      setProjects(data.projects ?? [])
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

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-semibold text-zinc-100">Projects</h2>
          <p className="text-sm text-zinc-500">
            {projects.length} project{projects.length === 1 ? '' : 's'} tracked
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

      {loading ? (
        <div className="text-sm text-zinc-500">Loading…</div>
      ) : (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {projects.map((p) => (
            <article
              key={p.id}
              className="rounded-xl border border-zinc-800 bg-zinc-900/70 p-4 shadow-sm"
            >
              <div className="mb-2 flex items-start justify-between gap-2">
                <div>
                  <h3 className="text-base font-semibold text-zinc-100">
                    {p.display_name}
                  </h3>
                  <p className="text-[11px] text-zinc-500">{p.id}</p>
                </div>
                <span className="rounded-full border border-zinc-700 bg-zinc-800 px-2 py-0.5 text-[11px] text-zinc-400">
                  {p.status}
                </span>
              </div>

              {p.repo_url && (
                <p className="mb-2 truncate text-[11px] font-mono text-sky-400">
                  {p.repo_url}
                </p>
              )}

              <div className="mb-3 rounded-lg bg-zinc-950/60 p-2">
                <div className="flex items-center justify-between text-[11px] text-zinc-500">
                  <span>{p.default_branch}</span>
                  <span>{timeAgo(p.main_last_synced_at)}</span>
                </div>
                <p className="mt-1 truncate font-mono text-xs text-zinc-300">
                  {p.main_commit_sha ? p.main_commit_sha.slice(0, 10) : '—'}
                </p>
                {p.main_commit_message && (
                  <p className="mt-1 truncate text-xs text-zinc-400">
                    {p.main_commit_message}
                  </p>
                )}
                {p.main_committed_by && (
                  <p className="text-[10px] text-zinc-500">
                    by {p.main_committed_by}
                    {p.main_committed_at ? ` · ${timeAgo(p.main_committed_at)}` : ''}
                  </p>
                )}
              </div>

              {p.environments.length > 0 && (
                <div className="mb-2">
                  <p className="mb-1 text-[10px] uppercase tracking-wide text-zinc-500">
                    Environments
                  </p>
                  <div className="flex flex-wrap gap-1">
                    {p.environments.map((e) => (
                      <span
                        key={e.id}
                        className={`rounded-full border px-2 py-0.5 text-[10px] ${envBadge(e.health_status ?? e.deploy_status)}`}
                        title={
                          e.deployed_commit_sha
                            ? `${e.name} @ ${e.deployed_commit_sha.slice(0, 10)}`
                            : e.name
                        }
                      >
                        {e.name}
                        {e.health_status ? ` · ${e.health_status}` : ''}
                      </span>
                    ))}
                  </div>
                </div>
              )}

              <div className="flex items-center justify-between text-[11px] text-zinc-500">
                <span>{p.active_branch_count} active branches</span>
                <span>{p.target_computers.length} targets</span>
              </div>
            </article>
          ))}
        </div>
      )}
      {!loading && projects.length === 0 && (
        <div className="rounded-xl border border-zinc-800 bg-zinc-900/40 p-8 text-center text-sm text-zinc-500">
          No projects configured.
        </div>
      )}
    </section>
  )
}
