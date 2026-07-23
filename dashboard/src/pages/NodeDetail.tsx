import { type ReactNode, useCallback, useEffect, useMemo, useState } from 'react'
import { useNavigate, useParams } from 'react-router-dom'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { getJson } from '../lib/api'
import { extractNodes } from '../lib/normalizers'
import { cn } from '../lib/utils'
import type { FleetComputer, FleetStatusResponse } from '../types'

export function NodeDetail() {
  const { nodeId = '' } = useParams()
  const navigate = useNavigate()
  const [node, setNode] = useState<FleetComputer | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      const direct = await getJson<FleetComputer>(`/api/fleet/nodes/${encodeURIComponent(nodeId)}`).catch(
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

  const status = node ? String(node.status ?? node.health ?? 'unknown') : 'unknown'
  const leaderState = node ? node.leader_state ?? (node.is_leader ? 'leader' : 'follower') : 'unknown'
  const sourceKind = node ? node.source_kind ?? (node.runtime_enrolled ? 'enrolled/live' : 'seed/static') : 'unknown'

  return (
    <section className="min-h-full space-y-6 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Node Detail</h1>
            {node ? <StatusBadge status={status}>{status}</StatusBadge> : null}
            {node ? <Badge variant={node.is_leader ? 'default' : 'neutral'}>{leaderState}</Badge> : null}
          </div>
          <p className="mt-1 truncate text-sm text-dim">
            {node ? `${node.name} - ${node.ip ?? node.hostname ?? 'unknown endpoint'}` : nodeId}
          </p>
        </div>
        <Button variant="outline" onClick={() => navigate('/')}>
          Back to fleet
        </Button>
      </div>

      {loading ? <Info text="Loading node details..." /> : null}
      {error ? <Info text={`Error: ${error}`} danger /> : null}

      {node ? (
        <div className="grid gap-4 lg:grid-cols-2">
          <Panel
            title="Identity & Role"
            description="Inventory identity, routing endpoint, leadership, and runtime source"
            action={<Badge variant={sourceKind === 'enrolled/live' ? 'info' : 'neutral'}>{sourceKind}</Badge>}
          >
            <Field label="Name" value={node.name} />
            <Field label="IP" value={node.ip} />
            <Field label="Role" value={node.role} />
            <Field label="Leader State" value={leaderState} />
            <Field label="Status" value={<StatusBadge status={status}>{status}</StatusBadge>} />
            <Field label="Source" value={sourceKind} />
            <Field label="Service Version" value={node.service_version ?? 'unreported'} />
          </Panel>

          <Panel title="Resources" description="Reported hardware and heartbeat freshness">
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

          <Panel
            title="Replication"
            description="Local replication mode, health, sequence, and detail"
            action={<StatusBadge status={node.replication_state?.health ?? 'unknown'} />}
          >
            <Field label="Mode" value={node.replication_state?.mode ?? 'unknown'} />
            <Field
              label="Health"
              value={<StatusBadge status={node.replication_state?.health ?? 'unknown'} />}
            />
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

          <Panel
            title="Current Workload"
            description="Scheduler activity and active task allocation"
            action={<StatusBadge status={node.current_workload?.status ?? 'unreported'} />}
          >
            <Field
              label="Status"
              value={<StatusBadge status={node.current_workload?.status ?? 'unreported'} />}
            />
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

          <Panel
            title="Models Loaded"
            description="Model inventory reported by the node"
            action={<Badge variant="neutral">{modelNames.length} models</Badge>}
          >
            {modelNames.length === 0 ? (
              <p className="text-sm text-dim">
                {node.models_loaded_state === 'unreported' ? 'unreported' : 'none'}
              </p>
            ) : (
              <div className="flex flex-wrap gap-1.5">
                {modelNames.map((name) => (
                  <Badge key={name} variant="neutral">
                    {name}
                  </Badge>
                ))}
              </div>
            )}
          </Panel>

          <Panel title="Runtime Provenance" description="Signals used to classify this node">
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

          <Panel
            title="Raw Metrics"
            description="Unmodified metrics payload returned by the fleet API"
            className="lg:col-span-2"
          >
            <JsonBlock data={node.metrics ?? {}} />
          </Panel>
        </div>
      ) : null}
    </section>
  )
}

function Panel({
  title,
  description,
  action,
  className,
  children,
}: {
  title: string
  description?: string
  action?: ReactNode
  className?: string
  children: ReactNode
}) {
  return (
    <Card className={cn('bg-panel', className)}>
      <CardHeader className="items-start gap-3">
        <div className="min-w-0">
          <CardTitle>{title}</CardTitle>
          {description ? <CardDescription className="mt-1">{description}</CardDescription> : null}
        </div>
        {action ? <div className="flex shrink-0 items-center gap-2">{action}</div> : null}
      </CardHeader>
      <div className="grid gap-3 sm:grid-cols-2">{children}</div>
    </Card>
  )
}

function Field({ label, value }: { label: string; value: ReactNode }) {
  const display = typeof value === 'string' && value.trim().length > 0 ? value : String(value ?? 'unknown')

  return (
    <div className="min-w-0 rounded-lg border border-border bg-surface px-3 py-2">
      <div className="text-xs text-dim">{label}</div>
      <div className="mt-1 min-w-0 wrap-break-word text-sm text-muted">
        {typeof value === 'string' || value == null ? display : value}
      </div>
    </div>
  )
}

function JsonBlock({ data }: { data: unknown }) {
  return (
    <pre className="max-h-80 overflow-auto rounded-lg border border-border bg-background p-3 text-xs text-muted">
      {JSON.stringify(data, null, 2)}
    </pre>
  )
}

function Info({ text, danger = false }: { text: string; danger?: boolean }) {
  return (
    <Card className={cn('bg-panel', danger ? 'border-status-crit' : 'border-border')}>
      <div className={cn('text-sm', danger ? 'text-status-crit' : 'text-muted')}>{text}</div>
    </Card>
  )
}
