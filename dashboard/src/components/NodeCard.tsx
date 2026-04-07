import { Link } from 'react-router-dom'
import type { FleetNode } from '../types'

type NodeCardProps = {
  node: FleetNode
}

function statusClass(status: string) {
  switch (status) {
    case 'online':
    case 'healthy':
      return 'bg-emerald-500/20 text-emerald-200'
    case 'degraded':
      return 'bg-amber-500/20 text-amber-200'
    case 'offline':
      return 'bg-rose-500/20 text-rose-200'
    default:
      return 'bg-slate-700 text-slate-200'
  }
}

function sourceClass(source: string) {
  if (source === 'enrolled/live') return 'bg-sky-500/20 text-sky-200'
  if (source === 'seed/static') return 'bg-slate-700 text-slate-200'
  return 'bg-violet-500/20 text-violet-200'
}

function formatWorkload(node: FleetNode): string {
  const workload = node.current_workload
  if (!workload) return 'unreported'
  if (workload.active_tasks == null) return workload.status
  if (workload.active_tasks === 0) return 'idle'
  return `${workload.active_tasks} active`
}

function formatHeartbeat(node: FleetNode): string {
  const heartbeat = node.last_heartbeat ?? 'unknown'
  const freshness = node.heartbeat_freshness ?? 'unknown'
  if (!node.heartbeat_age_seconds && freshness === 'unknown') return heartbeat

  const age =
    typeof node.heartbeat_age_seconds === 'number'
      ? `${node.heartbeat_age_seconds}s`
      : 'age unknown'
  return `${heartbeat} (${freshness}, ${age})`
}

export function NodeCard({ node }: NodeCardProps) {
  const status = (node.status ?? node.health ?? 'unknown').toLowerCase()
  const sourceKind = node.source_kind ?? (node.runtime_enrolled ? 'enrolled/live' : 'seed/static')
  const name = node.name ?? node.id ?? 'Unnamed node'
  const isLeader = node.is_leader || node.leader_state === 'leader'

  const modelsLoaded = node.models_loaded ?? []
  const modelsText =
    node.models_loaded_state === 'unreported'
      ? 'unreported'
      : `${modelsLoaded.length} (${modelsLoaded.join(', ') || 'none'})`

  return (
    <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4 shadow-sm">
      <div className="mb-3 flex items-center justify-between gap-2">
        <div>
          <h3 className="text-base font-semibold text-slate-100">{name}</h3>
          <p className="text-xs text-slate-400">
            {node.ip ?? 'unknown ip'} • {node.role ?? 'unknown role'}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <span className={`rounded-full px-2 py-1 text-xs ${statusClass(status)}`}>{status}</span>
          <span className="rounded-full bg-slate-800 px-2 py-1 text-xs text-slate-300">
            {isLeader ? 'leader' : node.leader_state ?? 'follower'}
          </span>
          <span className={`rounded-full px-2 py-1 text-xs ${sourceClass(sourceKind)}`}>{sourceKind}</span>
        </div>
      </div>

      <dl className="grid grid-cols-2 gap-2 text-sm">
        <Field label="CPU" value={node.cpu ?? node.hardware?.cpu ?? 'unknown'} />
        <Field label="RAM" value={node.ram ?? node.hardware?.ram ?? 'unknown'} />
        <Field label="GPU" value={node.gpu ?? node.hardware?.gpu ?? 'unknown'} />
        <Field label="Service" value={node.service_version ?? 'unreported'} />
        <Field label="Heartbeat" value={formatHeartbeat(node)} />
        <Field
          label="Replication"
          value={`${node.replication_state?.mode ?? 'unknown'} / ${node.replication_state?.health ?? 'unknown'}`}
        />
        <Field label="Workload" value={formatWorkload(node)} />
        <Field label="Heartbeat Source" value={node.heartbeat_source ?? 'unreported'} />
      </dl>

      <p className="mt-3 text-xs text-slate-400">Models loaded: {modelsText}</p>
      <p className="mt-1 text-xs text-slate-500">
        Runtime provenance: {(node.runtime_provenance ?? []).join(', ') || 'unreported'}
      </p>

      <Link
        to={`/nodes/${encodeURIComponent(node.id ?? node.name)}`}
        className="mt-4 inline-block text-sm text-sky-300 hover:text-sky-200"
      >
        View details →
      </Link>
    </article>
  )
}

function Field({ label, value }: { label: string; value: string | number | undefined }) {
  return (
    <div>
      <dt className="text-slate-500">{label}</dt>
      <dd className="text-slate-200">{value == null || value === '' ? 'unknown' : String(value)}</dd>
    </div>
  )
}
