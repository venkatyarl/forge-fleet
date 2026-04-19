import { useCallback, useEffect, useState } from 'react'
import { getJson } from '../lib/api'

type Leader = {
  computer_id: string
  member_name: string
  epoch: number
  elected_at?: string | null
  reason?: string | null
  heartbeat_at?: string | null
  heartbeat_age_seconds?: number | null
  primary_ip?: string | null
  status?: string | null
}

type Candidate = {
  computer_id: string
  name: string
  primary_ip: string
  status: string
  role: string
  election_priority: number
  last_seen_at?: string | null
}

function heartbeatColor(age?: number | null): string {
  if (age == null) return 'text-zinc-400'
  if (age < 30) return 'text-emerald-400'
  if (age < 120) return 'text-amber-400'
  return 'text-rose-400'
}

export function LeaderPanel() {
  const [leader, setLeader] = useState<Leader | null>(null)
  const [candidates, setCandidates] = useState<Candidate[]>([])
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const load = useCallback(async () => {
    try {
      setError(null)
      const data = await getJson<{ leader: Leader | null; candidates: Candidate[] }>(
        '/api/fleet/leader',
      )
      setLeader(data.leader ?? null)
      setCandidates(data.candidates ?? [])
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
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-semibold text-zinc-100">Leader Election</h2>
          <p className="text-sm text-zinc-500">Current leader + candidate ranking</p>
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
      ) : !leader ? (
        <div className="rounded-xl border border-amber-500/20 bg-amber-500/5 px-4 py-3 text-sm text-amber-300">
          No leader elected. Candidates below.
        </div>
      ) : (
        <article className="rounded-xl border border-violet-500/30 bg-violet-500/5 p-5">
          <div className="flex items-center justify-between">
            <div>
              <p className="text-xs uppercase tracking-wide text-violet-400">Leader</p>
              <h3 className="mt-1 text-2xl font-semibold text-zinc-100">
                {leader.member_name}
              </h3>
              <p className="text-xs text-zinc-500">
                {leader.primary_ip ?? '—'} • epoch {leader.epoch}
              </p>
            </div>
            <div className="text-right">
              <p className="text-xs uppercase tracking-wide text-zinc-500">Heartbeat</p>
              <p className={`mt-1 text-lg font-semibold ${heartbeatColor(leader.heartbeat_age_seconds)}`}>
                {leader.heartbeat_age_seconds == null ? '—' : `${leader.heartbeat_age_seconds}s ago`}
              </p>
              <p className="text-xs text-zinc-500">
                {leader.heartbeat_at ? new Date(leader.heartbeat_at).toLocaleTimeString() : '—'}
              </p>
            </div>
          </div>
          {leader.reason && (
            <p className="mt-3 text-sm text-zinc-400">
              <span className="text-zinc-500">reason:</span> {leader.reason}
            </p>
          )}
          {leader.elected_at && (
            <p className="mt-1 text-xs text-zinc-500">
              elected {new Date(leader.elected_at).toLocaleString()}
            </p>
          )}
        </article>
      )}

      <div>
        <h3 className="mb-2 text-sm font-semibold uppercase tracking-wide text-zinc-400">
          Candidates (by priority)
        </h3>
        <div className="overflow-hidden rounded-xl border border-zinc-800">
          <table className="w-full text-sm">
            <thead className="bg-zinc-900/80 text-left text-xs uppercase tracking-wide text-zinc-500">
              <tr>
                <th className="px-3 py-2">#</th>
                <th className="px-3 py-2">Name</th>
                <th className="px-3 py-2">IP</th>
                <th className="px-3 py-2">Role</th>
                <th className="px-3 py-2">Priority</th>
                <th className="px-3 py-2">Status</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-zinc-800">
              {candidates.map((c, i) => {
                const isLeader = leader && leader.computer_id === c.computer_id
                return (
                  <tr
                    key={c.computer_id}
                    className={isLeader ? 'bg-violet-500/5' : 'hover:bg-zinc-900/40'}
                  >
                    <td className="px-3 py-2 text-zinc-500">{i + 1}</td>
                    <td className="px-3 py-2 text-zinc-100">
                      {c.name}{' '}
                      {isLeader && (
                        <span className="ml-1 rounded bg-violet-500/20 px-1.5 py-0.5 text-[10px] text-violet-300">
                          leader
                        </span>
                      )}
                    </td>
                    <td className="px-3 py-2 text-zinc-400">{c.primary_ip}</td>
                    <td className="px-3 py-2 text-zinc-400">{c.role}</td>
                    <td className="px-3 py-2 text-zinc-300">{c.election_priority}</td>
                    <td className="px-3 py-2 text-zinc-400">{c.status}</td>
                  </tr>
                )
              })}
            </tbody>
          </table>
          {candidates.length === 0 && (
            <div className="p-4 text-center text-sm text-zinc-500">No candidates</div>
          )}
        </div>
      </div>
    </section>
  )
}
