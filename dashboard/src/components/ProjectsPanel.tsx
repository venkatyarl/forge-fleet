import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'
import { PanelHeader, RefreshButton } from './PanelHeader'
import { StatusBadge, toneFor } from './StatusBadge'

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

function envTone(status?: string | null) {
  return toneFor(status ?? '')
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
      <PanelHeader
        title="Projects"
        subtitle={`${projects.length} project${projects.length === 1 ? '' : 's'} tracked`}
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
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {projects.map((p) => (
            <article
              key={p.id}
              className="rounded-xl border border-zinc-800 bg-zinc-900/50 p-4 shadow-sm"
            >
              <div className="mb-2 flex items-start justify-between gap-2">
                <div>
                  <h3 className="text-base font-semibold text-zinc-100">
                    {p.display_name}
                  </h3>
                  <p className="text-[11px] text-zinc-500">{p.id}</p>
                </div>
                <StatusBadge status={p.status} />
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
                  <p className="mb-1 text-[10px] uppercase tracking-wider text-zinc-500">
                    Environments
                  </p>
                  <div className="flex flex-wrap gap-1">
                    {p.environments.map((e) => (
                      <StatusBadge
                        key={e.id}
                        tone={envTone(e.health_status ?? e.deploy_status)}
                        title={
                          e.deployed_commit_sha
                            ? `${e.name} @ ${e.deployed_commit_sha.slice(0, 10)}`
                            : e.name
                        }
                      >
                        {e.name}
                        {e.health_status ? ` · ${e.health_status}` : ''}
                      </StatusBadge>
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
