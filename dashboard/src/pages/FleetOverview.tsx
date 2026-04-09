import { useCallback, useEffect, useMemo, useState } from 'react'
import { useOutletContext } from 'react-router-dom'
import { NodeCard } from '../components/NodeCard'
import { SkeletonStatCard, SkeletonCard } from '../components/Skeleton'
import { EmptyState } from '../components/EmptyState'
import { getJson } from '../lib/api'
import { extractNodes, extractSummary } from '../lib/normalizers'
import type { FleetNode, FleetStatusResponse, WsEvent } from '../types'

type HealthResponse = { status?: string; [key: string]: unknown }

export function FleetOverview() {
  
  const { wsEvent } = useOutletContext<{ wsEvent: WsEvent | null }>()
  const [nodes, setNodes] = useState<FleetNode[]>([])
  const [gatewayHealth, setGatewayHealth] = useState('unknown')
  const [summary, setSummary] = useState({
    total_nodes: 0, connected_nodes: 0, unhealthy_nodes: 0,
    enrolled_nodes: 0, seed_nodes: 0, model_count: 0, leader: 'unknown',
  })
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const [fleet, healthData] = await Promise.all([
        getJson<FleetStatusResponse>('/api/fleet/status').catch(() => getJson<FleetStatusResponse>('/api/status')),
        getJson<HealthResponse>('/health').catch(() => ({ status: 'unknown' })),
      ])
      // Filter out phantom/test nodes
      const allNodes = extractNodes(fleet)
      setNodes(allNodes.filter(n => {
        const name = (n.name ?? '').toLowerCase()
        return !name.includes('postgres-verify') && !name.includes('verify.local')
      }))
      const s = extractSummary(fleet)
      setSummary({
        total_nodes: s.total_nodes ?? 0, connected_nodes: s.connected_nodes ?? 0,
        unhealthy_nodes: s.unhealthy_nodes ?? 0, enrolled_nodes: s.enrolled_nodes ?? 0,
        seed_nodes: s.seed_nodes ?? 0, model_count: s.model_count ?? 0,
        leader: s.leader ?? 'unknown',
      })
      setGatewayHealth(healthData.status ?? 'unknown')
      setLastUpdated(new Date())
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => { void load(); const i = setInterval(() => void load(), 15000); return () => clearInterval(i) }, [load])
  useEffect(() => { if (wsEvent) void load() }, [wsEvent, load])

  const leaderName = useMemo(() => {
    return nodes.find(n => n.is_leader || n.leader_state === 'leader')?.name ?? summary.leader
  }, [nodes, summary.leader])

  const healthPct = summary.total_nodes > 0
    ? Math.round(((summary.total_nodes - summary.unhealthy_nodes) / summary.total_nodes) * 100) : 0

  return (
    <section className="space-y-6">
      {/* Status Banner */}
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-semibold text-zinc-100">Fleet Overview</h2>
          <p className="text-sm text-zinc-500">
            {lastUpdated ? `Updated ${lastUpdated.toLocaleTimeString()}` : 'Loading...'}
          </p>
        </div>
        <div className="flex items-center gap-3">
          <div className={`flex items-center gap-2 rounded-full px-3 py-1.5 text-sm font-medium ${
            healthPct >= 80 ? 'bg-emerald-500/10 text-emerald-400 border border-emerald-500/20' :
            healthPct >= 50 ? 'bg-amber-500/10 text-amber-400 border border-amber-500/20' :
            'bg-rose-500/10 text-rose-400 border border-rose-500/20'
          }`}>
            <span className={`h-2 w-2 rounded-full ${healthPct >= 80 ? 'bg-emerald-400' : healthPct >= 50 ? 'bg-amber-400' : 'bg-rose-400'}`} />
            {healthPct}% Healthy
          </div>
          <button onClick={() => void load()} className="rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-1.5 text-sm text-zinc-400 hover:text-zinc-200 transition">
            Refresh
          </button>
        </div>
      </div>

      {/* Golden Signals — 4 key metrics */}
      {loading ? (
        <div className="grid gap-3 md:grid-cols-4">
          {[1,2,3,4].map(i => <SkeletonStatCard key={i} />)}
        </div>
      ) : (
        <div className="grid gap-3 md:grid-cols-4">
          <SignalCard label="Members Online" value={`${summary.connected_nodes}/${summary.total_nodes}`} color="emerald" icon="🖥️" />
          <SignalCard label="Models Loaded" value={String(summary.model_count)} color="sky" icon="🤖" />
          <SignalCard label="Unhealthy" value={String(summary.unhealthy_nodes)} color={summary.unhealthy_nodes > 0 ? 'rose' : 'zinc'} icon="⚠️" />
          <SignalCard label="Leader" value={leaderName} color="violet" icon="👑" />
        </div>
      )}

      {/* Secondary stats */}
      <div className="grid gap-3 md:grid-cols-4">
        <MiniStat label="Enrolled" value={summary.enrolled_nodes} />
        <MiniStat label="Seed" value={summary.seed_nodes} />
        <MiniStat label="Gateway" value={gatewayHealth} />
        <MiniStat label="Version" value="v2026.4.7" />
      </div>

      {error && (
        <div className="rounded-xl border border-rose-500/20 bg-rose-500/5 px-4 py-3 text-sm text-rose-300">
          {error}
        </div>
      )}

      {/* Node Grid */}
      {loading ? (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {[1,2,3,4,5,6].map(i => <SkeletonCard key={i} lines={4} />)}
        </div>
      ) : nodes.length === 0 ? (
        <EmptyState
          icon="🖥️"
          title="No fleet members connected"
          description="Add your first fleet member to start monitoring. Each member runs ForgeFleet daemon with LLM inference."
          primaryAction={{ label: 'Add Fleet Member', to: '/onboarding' }}
          secondaryAction={{ label: 'Setup Guide', to: '/onboarding' }}
        />
      ) : (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {nodes.map(node => <NodeCard key={node.id ?? node.name} node={node} />)}
        </div>
      )}
    </section>
  )
}

function SignalCard({ label, value, color, icon }: { label: string; value: string; color: string; icon: string }) {
  const colors: Record<string, string> = {
    emerald: 'border-emerald-500/20 text-emerald-400',
    sky: 'border-sky-500/20 text-sky-400',
    rose: 'border-rose-500/20 text-rose-400',
    violet: 'border-violet-500/20 text-violet-400',
    zinc: 'border-zinc-700 text-zinc-400',
  }
  return (
    <article className={`rounded-xl border bg-zinc-900/80 p-4 ${colors[color] || colors.zinc}`}>
      <div className="flex items-center justify-between">
        <p className="text-xs uppercase tracking-wide text-zinc-500">{label}</p>
        <span className="text-lg">{icon}</span>
      </div>
      <p className={`mt-2 text-2xl font-semibold ${colors[color]?.split(' ')[1] || 'text-zinc-100'}`}>{value}</p>
    </article>
  )
}

function MiniStat({ label, value }: { label: string; value: string | number }) {
  return (
    <div className="flex items-center justify-between rounded-lg border border-zinc-800 bg-zinc-900/50 px-3 py-2">
      <span className="text-xs text-zinc-500">{label}</span>
      <span className="text-sm font-medium text-zinc-300">{String(value)}</span>
    </div>
  )
}
