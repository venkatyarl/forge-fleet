import { useCallback, useEffect, useMemo, useState } from 'react'
import { useOutletContext } from 'react-router-dom'
import { NodeCard } from '../components/NodeCard'
import { getJson } from '../lib/api'
import { extractNodes, extractSummary } from '../lib/normalizers'
import type { FleetNode, FleetStatusResponse, WsEvent } from '../types'

type HealthResponse = {
  status?: string
  [key: string]: unknown
}

export function FleetOverview() {
  const { wsEvent } = useOutletContext<{ wsEvent: WsEvent | null }>()
  const [nodes, setNodes] = useState<FleetNode[]>([])
  const [gatewayHealth, setGatewayHealth] = useState('unknown')
  const [summary, setSummary] = useState({
    total_nodes: 0,
    connected_nodes: 0,
    unhealthy_nodes: 0,
    enrolled_nodes: 0,
    seed_nodes: 0,
    model_count: 0,
    leader: 'unknown',
  })
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const [fleet, healthData] = await Promise.all([
        getJson<FleetStatusResponse>('/api/fleet/status').catch(() =>
          getJson<FleetStatusResponse>('/api/status'),
        ),
        getJson<HealthResponse>('/health').catch(() => ({ status: 'unknown' })),
      ])

      const normalizedNodes = extractNodes(fleet)
      const normalizedSummary = extractSummary(fleet)

      setNodes(normalizedNodes)
      setSummary({
        total_nodes: normalizedSummary.total_nodes ?? normalizedNodes.length,
        connected_nodes: normalizedSummary.connected_nodes ?? 0,
        unhealthy_nodes: normalizedSummary.unhealthy_nodes ?? 0,
        enrolled_nodes: normalizedSummary.enrolled_nodes ?? 0,
        seed_nodes: normalizedSummary.seed_nodes ?? 0,
        model_count: normalizedSummary.model_count ?? 0,
        leader: normalizedSummary.leader ?? 'unknown',
      })
      setGatewayHealth(healthData.status ?? 'unknown')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load fleet overview')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
    const interval = window.setInterval(() => void load(), 15000)
    return () => window.clearInterval(interval)
  }, [load])

  useEffect(() => {
    if (wsEvent) {
      void load()
    }
  }, [wsEvent, load])

  const leaderNodeName = useMemo(() => {
    const explicitLeader = nodes.find((node) => node.is_leader || node.leader_state === 'leader')
    return explicitLeader?.name ?? summary.leader
  }, [nodes, summary.leader])

  return (
    <section className="space-y-4">
      <article className="rounded-2xl border border-slate-800 bg-gradient-to-r from-slate-900 via-slate-900 to-slate-950 p-4 md:p-5">
        <div className="flex flex-wrap items-center gap-4">
          <img
            src="/brand/forgefleet-mark.svg"
            alt="ForgeFleet"
            className="h-14 w-14 rounded-xl border border-slate-700/80 bg-slate-950/80 p-1.5"
          />
          <div>
            <h2 className="text-base font-semibold text-slate-100 md:text-lg">ForgeFleet Command Mesh</h2>
            <p className="mt-1 text-sm text-slate-300">
              Unified fleet telemetry, mission control, and routing visibility in one dark-first command center.
            </p>
          </div>
        </div>
      </article>

      <div className="grid gap-3 md:grid-cols-3 xl:grid-cols-8">
        <Stat label="Total Nodes" value={String(summary.total_nodes)} />
        <Stat label="Connected" value={String(summary.connected_nodes)} accent="text-emerald-300" />
        <Stat label="Unhealthy" value={String(summary.unhealthy_nodes)} accent="text-amber-300" />
        <Stat label="Live Enrolled" value={String(summary.enrolled_nodes)} accent="text-sky-300" />
        <Stat label="Seed Static" value={String(summary.seed_nodes)} accent="text-slate-300" />
        <Stat label="Models Loaded" value={String(summary.model_count)} accent="text-sky-300" />
        <Stat label="Leader" value={leaderNodeName} />
        <Stat label="Gateway Health" value={gatewayHealth} />
      </div>

      {loading ? <PanelText text="Loading fleet overview..." /> : null}
      {error ? <PanelText text={`Error: ${error}`} danger /> : null}

      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
        {nodes.map((node) => (
          <NodeCard key={node.id ?? node.name} node={node} />
        ))}
      </div>

      {!loading && nodes.length === 0 ? (
        <PanelText text="No nodes reported by API yet." />
      ) : null}
    </section>
  )
}

function Stat({ label, value, accent }: { label: string; value: string; accent?: string }) {
  return (
    <article className="rounded-xl border border-slate-800 bg-slate-900/70 p-4">
      <p className="text-xs uppercase tracking-wide text-slate-500">{label}</p>
      <p className={`mt-2 text-2xl font-semibold ${accent ?? 'text-slate-100'}`}>{value}</p>
    </article>
  )
}

function PanelText({ text, danger = false }: { text: string; danger?: boolean }) {
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
