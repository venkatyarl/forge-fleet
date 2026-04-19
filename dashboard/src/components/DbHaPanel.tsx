import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'

type Replica = {
  computer_id: string
  computer_name: string
  primary_ip: string
  database_kind: string
  role: string
  status: string
  lag_bytes?: number | null
  last_sync_at?: string | null
  promoted_at?: string | null
  notes?: string | null
}

type Backup = {
  id: string
  database_kind: string
  created_at: string
  age_seconds: number
  size_bytes: number
  source_computer_name?: string | null
  file_name: string
  verified_restorable_at?: string | null
  retention_tier: string
  distribution_status: Record<string, string>
}

function bytes(n?: number | null): string {
  if (n == null) return '—'
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} K`
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} M`
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} G`
}

function ageString(s?: number | null): string {
  if (s == null) return '—'
  if (s < 60) return `${s}s ago`
  if (s < 3600) return `${Math.floor(s / 60)}m ago`
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`
  return `${Math.floor(s / 86400)}d ago`
}

function roleBadge(role: string): string {
  switch (role) {
    case 'primary':
      return 'bg-violet-500/15 text-violet-300 border-violet-500/30'
    case 'replica':
      return 'bg-sky-500/15 text-sky-300 border-sky-500/30'
    case 'sentinel_voter':
      return 'bg-amber-500/15 text-amber-300 border-amber-500/30'
    default:
      return 'bg-zinc-800 text-zinc-400 border-zinc-700'
  }
}

function statusBadge(status: string): string {
  switch (status) {
    case 'running':
      return 'bg-emerald-500/15 text-emerald-300 border-emerald-500/30'
    case 'syncing':
      return 'bg-sky-500/15 text-sky-300 border-sky-500/30'
    case 'promoting':
      return 'bg-amber-500/15 text-amber-300 border-amber-500/30'
    case 'failed':
    case 'stopped':
      return 'bg-rose-500/15 text-rose-300 border-rose-500/30'
    default:
      return 'bg-zinc-800 text-zinc-400 border-zinc-700'
  }
}

export function DbHaPanel() {
  const [replicas, setReplicas] = useState<Replica[]>([])
  const [backups, setBackups] = useState<Backup[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const load = useCallback(async () => {
    try {
      setError(null)
      const data = await getJson<{ replicas: Replica[]; backups: Backup[] }>(
        '/api/ha/status',
      )
      setReplicas(data.replicas ?? [])
      setBackups(data.backups ?? [])
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
    <section className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-semibold text-zinc-100">Database HA</h2>
          <p className="text-sm text-zinc-500">Replication + backups</p>
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

      {/* Replicas */}
      <div>
        <h3 className="mb-2 text-sm font-semibold uppercase tracking-wide text-zinc-400">
          Replicas
        </h3>
        <div className="overflow-hidden rounded-xl border border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-900/80 text-left text-xs uppercase tracking-wide text-zinc-500">
              <tr>
                <th className="px-3 py-2">Kind</th>
                <th className="px-3 py-2">Role</th>
                <th className="px-3 py-2">Computer</th>
                <th className="px-3 py-2">Status</th>
                <th className="px-3 py-2">Lag</th>
                <th className="px-3 py-2">Last Sync</th>
                <th className="px-3 py-2">Notes</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-800">
              {replicas.map((r) => (
                <tr
                  key={`${r.database_kind}-${r.computer_id}`}
                  className="hover:bg-zinc-900/40"
                >
                  <td className="px-3 py-2 text-zinc-300">{r.database_kind}</td>
                  <td className="px-3 py-2">
                    <span
                      className={`rounded-full border px-2 py-0.5 text-[11px] ${roleBadge(r.role)}`}
                    >
                      {r.role}
                    </span>
                  </td>
                  <td className="px-3 py-2 text-zinc-100">
                    {r.computer_name}
                    <div className="text-[11px] text-zinc-500">{r.primary_ip}</div>
                  </td>
                  <td className="px-3 py-2">
                    <span
                      className={`rounded-full border px-2 py-0.5 text-[11px] ${statusBadge(r.status)}`}
                    >
                      {r.status}
                    </span>
                  </td>
                  <td className="px-3 py-2 text-zinc-400">{bytes(r.lag_bytes)}</td>
                  <td className="px-3 py-2 text-zinc-400">
                    {r.last_sync_at
                      ? new Date(r.last_sync_at).toLocaleTimeString()
                      : '—'}
                  </td>
                  <td className="px-3 py-2 text-zinc-500">{r.notes ?? '—'}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {!loading && replicas.length === 0 && (
            <div className="p-4 text-center text-sm text-zinc-500">
              No replica rows yet. Replication state is populated by the HA bootstrap flow.
            </div>
          )}
        </div>
      </div>

      {/* Backups */}
      <div>
        <h3 className="mb-2 text-sm font-semibold uppercase tracking-wide text-zinc-400">
          Recent Backups
        </h3>
        <div className="overflow-hidden rounded-xl border border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-900/80 text-left text-xs uppercase tracking-wide text-zinc-500">
              <tr>
                <th className="px-3 py-2">File</th>
                <th className="px-3 py-2">Kind</th>
                <th className="px-3 py-2">Source</th>
                <th className="px-3 py-2">Size</th>
                <th className="px-3 py-2">Age</th>
                <th className="px-3 py-2">Tier</th>
                <th className="px-3 py-2">Verified</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-800">
              {backups.map((b) => (
                <tr key={b.id} className="hover:bg-zinc-900/40">
                  <td className="px-3 py-2 font-mono text-xs text-zinc-300">
                    {b.file_name}
                  </td>
                  <td className="px-3 py-2 text-zinc-400">{b.database_kind}</td>
                  <td className="px-3 py-2 text-zinc-400">
                    {b.source_computer_name ?? '—'}
                  </td>
                  <td className="px-3 py-2 text-zinc-400">{bytes(b.size_bytes)}</td>
                  <td className="px-3 py-2 text-zinc-400">{ageString(b.age_seconds)}</td>
                  <td className="px-3 py-2 text-zinc-400">{b.retention_tier}</td>
                  <td className="px-3 py-2 text-zinc-400">
                    {b.verified_restorable_at ? (
                      <span className="text-emerald-300">✓</span>
                    ) : (
                      <span className="text-zinc-600">—</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {!loading && backups.length === 0 && (
            <div className="p-4 text-center text-sm text-zinc-500">
              No backups recorded yet.
            </div>
          )}
        </div>
      </div>
    </section>
  )
}
