import { type ReactNode, useCallback, useEffect, useMemo, useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { getJson } from '../lib/api'
import { extractNodes } from '../lib/normalizers'
import type { FleetNode, FleetStatusResponse } from '../types'

export function NodeDetail() {
  const { nodeId = '' } = useParams()
  const [node, setNode] = useState<FleetNode | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const direct = await getJson<FleetNode>(`/api/fleet/nodes/${encodeURIComponent(nodeId)}`).catch(
        () => null,
      )

      if (direct) {
        setNode(direct)
        return
      }

      const fleet = await getJson<FleetStatusResponse>('/api/fleet/status')
      const nodes = extractNodes(fleet)
      const match = nodes.find((candidate) => {
        const cid = String(candidate.id ?? candidate.name)
        return cid === nodeId || candidate.name === nodeId || candidate.ip === nodeId
      })

      setNode(match ?? null)
      if (!match) {
        setError(`Node ${nodeId} not found in fleet status.`)
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load node details')
    } finally {
      setLoading(false)
    }
  }, [nodeId])

  useEffect(() => {
    void load()
  }, [load])

  const modelNames = useMemo(() => {
    if (!node) return []
    if (node.models_loaded && node.models_loaded.length > 0) return node.models_loaded
    return (node.models ?? []).map((model) => model.name)
  }, [node])

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-xl font-semibold text-slate-100">Node Detail</h2>
        <Link to="/" className="text-sm text-sky-300 hover:text-sky-200">
          ← Back to fleet
        </Link>
      </div>

      {loading ? <Info text="Loading node details..." /> : null}
      {error ? <Info text={`Error: ${error}`} danger /> : null}

      {node ? (
        <div className="grid gap-4 lg:grid-cols-2">
          <Panel title="Identity & Role">
            <Field label="Name" value={node.name} />
            <Field label="IP" value={node.ip} />
            <Field label="Role" value={node.role} />
            <Field label="Leader State" value={node.leader_state ?? (node.is_leader ? 'leader' : 'follower')} />
            <Field label="Status" value={node.status ?? node.health} />
            <Field label="Source" value={node.source_kind ?? (node.runtime_enrolled ? 'enrolled/live' : 'seed/static')} />
            <Field label="Service Version" value={node.service_version ?? 'unreported'} />
          </Panel>

          <Panel title="Resources">
            <Field label="CPU" value={node.cpu ?? node.hardware?.cpu} />
            <Field label="RAM" value={node.ram ?? node.hardware?.ram} />
            <Field label="GPU" value={node.gpu ?? node.hardware?.gpu} />
            <Field label="Last Heartbeat" value={node.last_heartbeat ?? node.metrics?.checked_at ?? 'unknown'} />
            <Field label="Heartbeat Source" value={node.heartbeat_source ?? 'unreported'} />
            <Field
              label="Heartbeat Freshness"
              value={
                node.heartbeat_age_seconds == null
                  ? node.heartbeat_freshness ?? 'unknown'
                  : `${node.heartbeat_freshness ?? 'unknown'} (${node.heartbeat_age_seconds}s)`
              }
            />
          </Panel>

          <Panel title="Replication">
            <Field label="Mode" value={node.replication_state?.mode ?? 'unknown'} />
            <Field label="Health" value={node.replication_state?.health ?? 'unknown'} />
            <Field
              label="Sequence"
              value={
                node.replication_state?.sequence == null
                  ? 'unreported'
                  : String(node.replication_state.sequence)
              }
            />
            <Field label="Detail" value={node.replication_state?.detail ?? 'unreported'} />
          </Panel>

          <Panel title="Current Workload">
            <Field label="Status" value={node.current_workload?.status ?? 'unreported'} />
            <Field
              label="Active Tasks"
              value={
                node.current_workload?.active_tasks == null
                  ? 'unreported'
                  : String(node.current_workload.active_tasks)
              }
            />
            <Field
              label="Task IDs"
              value={
                node.current_workload?.task_ids && node.current_workload.task_ids.length > 0
                  ? node.current_workload.task_ids.join(', ')
                  : 'none'
              }
            />
          </Panel>

          <Panel title="Models Loaded">
            {modelNames.length === 0 ? (
              <p className="text-sm text-slate-400">
                {node.models_loaded_state === 'unreported' ? 'unreported' : 'none'}
              </p>
            ) : (
              <ul className="list-disc space-y-1 pl-5 text-sm text-slate-200">
                {modelNames.map((name) => (
                  <li key={name}>{name}</li>
                ))}
              </ul>
            )}
          </Panel>

          <Panel title="Runtime Provenance">
            <Field
              label="Signals"
              value={
                node.runtime_provenance && node.runtime_provenance.length > 0
                  ? node.runtime_provenance.join(', ')
                  : 'unreported'
              }
            />
            <Field label="Seeded from Config" value={node.seeded_from_config ? 'yes' : 'no'} />
            <Field label="Runtime Enrolled" value={node.runtime_enrolled ? 'yes' : 'no'} />
          </Panel>

          <Panel title="Raw Metrics">
            <JsonBlock data={node.metrics ?? {}} />
          </Panel>
        </div>
      ) : null}
    </section>
  )
}

function Panel({ title, children }: { title: string; children: ReactNode }) {
  return (
    <article className="rounded-xl border border-slate-800 bg-slate-900/70 p-4">
      <h3 className="mb-3 text-sm font-medium uppercase tracking-wide text-slate-400">{title}</h3>
      {children}
    </article>
  )
}

function Field({ label, value }: { label: string; value: unknown }) {
  const display = typeof value === 'string' && value.trim().length > 0 ? value : String(value ?? 'unknown')

  return (
    <div className="mb-1 text-sm">
      <span className="text-slate-500">{label}: </span>
      <span className="text-slate-200">{display}</span>
    </div>
  )
}

function JsonBlock({ data }: { data: unknown }) {
  return (
    <pre className="max-h-72 overflow-auto rounded-md bg-slate-950/80 p-3 text-xs text-slate-200">
      {JSON.stringify(data, null, 2)}
    </pre>
  )
}

function Info({ text, danger = false }: { text: string; danger?: boolean }) {
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
