import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { getJson } from '../lib/api'
import { extractNodes, extractSummary } from '../lib/normalizers'
import type { FleetNode, FleetStatusResponse } from '../types'

type RuntimeConfig = {
  nodes_configured?: number
  bootstrap_targets_configured?: number
}

type TokenState = {
  resolved?: boolean
  source?: string
}

type EnrollmentSettings = {
  default_role?: string | null
  allowed_roles?: string[]
  token?: TokenState
}

type BootstrapTarget = {
  name: string
  status?: string | null
  os?: string | null
  hardware?: string | null
  reachable_by_ssh?: boolean | null
  enrolled?: boolean | null
  required_manual_floor?: string[]
  notes?: string | null
}

type BootstrapSummary = {
  total_targets?: number
  enrolled_targets?: number
  ssh_reachable_targets?: number
  manual_steps_pending?: number
}

type SettingsRuntimeResponse = {
  runtime_config?: RuntimeConfig
  enrollment?: EnrollmentSettings
  bootstrap?: {
    summary?: BootstrapSummary
    targets?: BootstrapTarget[]
  }
}

type FleetSummaryState = {
  total_nodes: number
  connected_nodes: number
  enrolled_nodes: number
  seed_nodes: number
}

type ChecklistStep = {
  title: string
  detail: string
  routeLabel?: string
  routeTo?: string
}

const CHECKLIST_STEPS: ChecklistStep[] = [
  {
    title: 'Confirm enrollment trust gate is healthy',
    detail: 'Verify enrollment token source is resolved before adding any new node.',
    routeLabel: 'Open Settings',
    routeTo: '/settings',
  },
  {
    title: 'Validate static bootstrap inventory',
    detail: 'Make sure each planned machine appears as a seed/static node or in bootstrap targets.',
    routeLabel: 'Open Fleet Overview',
    routeTo: '/',
  },
  {
    title: 'Bring node online and enroll',
    detail: 'Use POST /api/fleet/enroll, then start heartbeats to promote the node from seed/static to enrolled/live.',
  },
  {
    title: 'Verify topology + replication visibility',
    detail: 'After enrollment, verify leader/follower role and replication health.',
    routeLabel: 'Open Topology',
    routeTo: '/topology',
  },
  {
    title: 'Inspect node-level runtime provenance',
    detail: 'Open node detail and confirm source kind, heartbeat freshness, workload, and service version.',
  },
]

function sourceLabel(node: FleetNode): string {
  return node.source_kind ?? (node.runtime_enrolled ? 'enrolled/live' : 'seed/static')
}

function sourcePillClass(source: string): string {
  if (source === 'enrolled/live') return 'bg-sky-500/20 text-sky-200 border border-sky-500/30'
  if (source === 'seed/static') return 'bg-slate-700 text-slate-200 border border-slate-600'
  return 'bg-violet-500/20 text-violet-200 border border-violet-500/30'
}

function boolLabel(value: boolean | null | undefined): string {
  if (value === true) return 'yes'
  if (value === false) return 'no'
  return 'unknown'
}

function boolPillClass(value: boolean | null | undefined): string {
  if (value === true) return 'bg-emerald-500/20 text-emerald-200 border border-emerald-500/30'
  if (value === false) return 'bg-rose-500/20 text-rose-200 border border-rose-500/30'
  return 'bg-slate-700 text-slate-200 border border-slate-600'
}

export function OperatorOnboarding() {
  const [nodes, setNodes] = useState<FleetNode[]>([])
  const [summary, setSummary] = useState<FleetSummaryState>({
    total_nodes: 0,
    connected_nodes: 0,
    enrolled_nodes: 0,
    seed_nodes: 0,
  })
  const [settings, setSettings] = useState<SettingsRuntimeResponse | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setError(null)
      setLoading(true)

      const [fleetPayload, settingsPayload] = await Promise.all([
        getJson<FleetStatusResponse>('/api/fleet/status').catch(() =>
          getJson<FleetStatusResponse>('/api/status'),
        ),
        getJson<SettingsRuntimeResponse>('/api/settings/runtime'),
      ])

      const normalizedNodes = extractNodes(fleetPayload)
      const normalizedSummary = extractSummary(fleetPayload)

      setNodes(normalizedNodes)
      setSummary({
        total_nodes: normalizedSummary.total_nodes ?? normalizedNodes.length,
        connected_nodes: normalizedSummary.connected_nodes ?? 0,
        enrolled_nodes: normalizedSummary.enrolled_nodes ?? 0,
        seed_nodes: normalizedSummary.seed_nodes ?? 0,
      })
      setSettings(settingsPayload)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load operator onboarding state')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  const enrolledNodes = useMemo(
    () => nodes.filter((node) => sourceLabel(node) === 'enrolled/live'),
    [nodes],
  )

  const seedNodes = useMemo(
    () => nodes.filter((node) => sourceLabel(node) === 'seed/static'),
    [nodes],
  )

  const bootstrapTargets = settings?.bootstrap?.targets ?? []
  const bootstrapSummary = settings?.bootstrap?.summary
  const enrollmentTokenReady = settings?.enrollment?.token?.resolved === true

  const configuredNodes = settings?.runtime_config?.nodes_configured ?? summary.total_nodes
  const configuredBootstrapTargets =
    settings?.runtime_config?.bootstrap_targets_configured ?? bootstrapTargets.length

  return (
    <section className="space-y-6">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight text-slate-100">Operator Onboarding & Bring-Up</h1>
          <p className="mt-1 text-sm text-slate-400">
            Real bring-up path for adding computers: checklist, bootstrap target visibility, and live seed/static → enrolled/live progress.
          </p>
        </div>
        <button
          type="button"
          onClick={() => void load()}
          disabled={loading}
          className="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"
        >
          {loading ? 'Refreshing…' : '↻ Refresh'}
        </button>
      </div>

      {error ? <Info text={`Error: ${error}`} danger /> : null}

      <Info
        text={
          enrollmentTokenReady
            ? `Enrollment token source is healthy (${settings?.enrollment?.token?.source ?? 'resolved'}). New nodes can enroll when ready.`
            : 'Enrollment token is not resolved. Bring-up will fail until enrollment secret is configured.'
        }
        accent={
          enrollmentTokenReady
            ? 'rounded-xl border border-emerald-500/30 bg-emerald-500/10 px-4 py-3 text-sm text-emerald-200'
            : 'rounded-xl border border-amber-500/30 bg-amber-500/10 px-4 py-3 text-sm text-amber-200'
        }
      />

      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-6">
        <Stat label="Configured Nodes" value={configuredNodes} />
        <Stat label="Connected" value={summary.connected_nodes} accent="text-emerald-300" />
        <Stat label="Live Enrolled" value={summary.enrolled_nodes} accent="text-sky-300" />
        <Stat label="Seed Static" value={summary.seed_nodes} />
        <Stat label="Bootstrap Targets" value={configuredBootstrapTargets} />
        <Stat label="SSH Reachable Targets" value={bootstrapSummary?.ssh_reachable_targets ?? 0} accent="text-emerald-300" />
      </div>

      <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
        <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">Onboarding Checklist</h2>
        <ol className="space-y-3">
          {CHECKLIST_STEPS.map((step, index) => (
            <li key={step.title} className="rounded-lg border border-slate-800 bg-slate-950/70 p-3">
              <p className="text-sm font-medium text-slate-100">
                {index + 1}. {step.title}
              </p>
              <p className="mt-1 text-sm text-slate-400">{step.detail}</p>
              {step.routeLabel && step.routeTo ? (
                <Link to={step.routeTo} className="mt-2 inline-block text-xs text-sky-300 hover:text-sky-200">
                  {step.routeLabel} →
                </Link>
              ) : null}
            </li>
          ))}
        </ol>
      </article>

      <div className="grid gap-4 xl:grid-cols-2">
        <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
          <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">
            Seed/Static Nodes Awaiting Bring-Up ({seedNodes.length})
          </h2>
          {seedNodes.length === 0 ? (
            <p className="text-sm text-slate-400">No seed/static nodes currently visible.</p>
          ) : (
            <ul className="space-y-2">
              {seedNodes.map((node) => (
                <NodeListItem key={node.id ?? node.name} node={node} />
              ))}
            </ul>
          )}
        </article>

        <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
          <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">
            Enrolled/Live Nodes ({enrolledNodes.length})
          </h2>
          {enrolledNodes.length === 0 ? (
            <p className="text-sm text-slate-400">No runtime-enrolled nodes reported yet.</p>
          ) : (
            <ul className="space-y-2">
              {enrolledNodes.map((node) => (
                <NodeListItem key={node.id ?? node.name} node={node} />
              ))}
            </ul>
          )}
        </article>
      </div>

      <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
        <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">
          Bootstrap Targets from Config ({bootstrapTargets.length})
        </h2>

        {bootstrapTargets.length === 0 ? (
          <p className="text-sm text-slate-400">
            No <code>[[bootstrap_targets]]</code> entries found in fleet config.
          </p>
        ) : (
          <div className="space-y-3">
            {bootstrapTargets.map((target) => {
              const manual = target.required_manual_floor ?? []
              const matchingNode = nodes.find((node) => {
                const id = String(node.id ?? node.name)
                return id === target.name || node.name === target.name || node.hostname === target.name
              })

              return (
                <div key={target.name} className="rounded-lg border border-slate-800 bg-slate-950/70 p-3">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <p className="text-sm font-medium text-slate-100">{target.name}</p>
                    <div className="flex flex-wrap items-center gap-2 text-xs">
                      <span className={`rounded-full px-2 py-0.5 ${boolPillClass(target.reachable_by_ssh)}`}>
                        ssh: {boolLabel(target.reachable_by_ssh)}
                      </span>
                      <span className={`rounded-full px-2 py-0.5 ${boolPillClass(target.enrolled)}`}>
                        enrolled: {boolLabel(target.enrolled)}
                      </span>
                      <span className="rounded-full border border-slate-700 bg-slate-800 px-2 py-0.5 text-slate-200">
                        status: {target.status ?? 'unreported'}
                      </span>
                    </div>
                  </div>

                  <p className="mt-1 text-xs text-slate-400">
                    OS: {target.os ?? 'unknown'} • Hardware: {target.hardware ?? 'unknown'}
                  </p>

                  {manual.length > 0 ? (
                    <ul className="mt-2 list-disc space-y-1 pl-5 text-xs text-amber-200">
                      {manual.map((step) => (
                        <li key={step}>{step}</li>
                      ))}
                    </ul>
                  ) : null}

                  {target.notes ? <p className="mt-2 text-xs text-slate-400">Notes: {target.notes}</p> : null}

                  {matchingNode ? (
                    <Link
                      to={`/nodes/${encodeURIComponent(matchingNode.id ?? matchingNode.name)}`}
                      className="mt-2 inline-block text-xs text-sky-300 hover:text-sky-200"
                    >
                      Open node detail →
                    </Link>
                  ) : null}
                </div>
              )
            })}
          </div>
        )}

        <div className="mt-4 grid gap-3 sm:grid-cols-3">
          <Stat label="Enrolled Targets" value={bootstrapSummary?.enrolled_targets ?? 0} accent="text-sky-300" compact />
          <Stat label="SSH Reachable" value={bootstrapSummary?.ssh_reachable_targets ?? 0} accent="text-emerald-300" compact />
          <Stat label="Manual Steps Pending" value={bootstrapSummary?.manual_steps_pending ?? 0} compact />
        </div>
      </article>

      <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
        <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">Bring-Up API Surface (No Placeholder Actions)</h2>
        <ul className="space-y-2 text-sm text-slate-300">
          <li>
            <code className="rounded bg-slate-950 px-1 py-0.5 text-xs">POST /api/fleet/enroll</code> — trust-gated runtime enrollment for a node
          </li>
          <li>
            <code className="rounded bg-slate-950 px-1 py-0.5 text-xs">POST /api/fleet/heartbeat</code> — runtime heartbeat updates after enrollment
          </li>
          <li>
            <code className="rounded bg-slate-950 px-1 py-0.5 text-xs">GET /api/fleet/status</code> — full fleet status with source_kind and runtime provenance
          </li>
          <li>
            <code className="rounded bg-slate-950 px-1 py-0.5 text-xs">GET /api/fleet/nodes/{'{id}'}</code> — direct node detail view
          </li>
          <li>
            <code className="rounded bg-slate-950 px-1 py-0.5 text-xs">GET /api/settings/runtime</code> — safe config visibility including bootstrap target summary
          </li>
        </ul>
      </article>
    </section>
  )
}

function NodeListItem({ node }: { node: FleetNode }) {
  const source = sourceLabel(node)
  const status = (node.status ?? node.health ?? 'unknown').toLowerCase()

  return (
    <li className="rounded-lg border border-slate-800 bg-slate-950/70 p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <p className="text-sm font-medium text-slate-100">{node.name}</p>
          <p className="text-xs text-slate-400">{node.ip ?? 'unknown ip'} • {node.role ?? 'unknown role'}</p>
        </div>
        <div className="flex items-center gap-2 text-xs">
          <span className="rounded-full border border-slate-700 bg-slate-800 px-2 py-0.5 text-slate-200">
            {status}
          </span>
          <span className={`rounded-full px-2 py-0.5 ${sourcePillClass(source)}`}>{source}</span>
        </div>
      </div>

      <p className="mt-1 text-xs text-slate-400">
        heartbeat: {node.last_heartbeat ?? 'unknown'} • freshness: {node.heartbeat_freshness ?? 'unknown'}
      </p>
      <p className="mt-1 text-xs text-slate-500">
        runtime provenance: {(node.runtime_provenance ?? []).join(', ') || 'unreported'}
      </p>

      <Link
        to={`/nodes/${encodeURIComponent(node.id ?? node.name)}`}
        className="mt-2 inline-block text-xs text-sky-300 hover:text-sky-200"
      >
        View details →
      </Link>
    </li>
  )
}

function Stat({
  label,
  value,
  accent,
  compact = false,
}: {
  label: string
  value: string | number
  accent?: string
  compact?: boolean
}) {
  return (
    <article className={`rounded-lg border border-slate-800 bg-slate-900/70 ${compact ? 'p-3' : 'p-4'}`}>
      <p className="text-xs uppercase tracking-wide text-slate-500">{label}</p>
      <p className={`${compact ? 'mt-1 text-xl' : 'mt-2 text-2xl'} font-semibold ${accent ?? 'text-slate-100'}`}>
        {value}
      </p>
    </article>
  )
}

function Info({
  text,
  danger = false,
  accent,
}: {
  text: string
  danger?: boolean
  accent?: string
}) {
  return (
    <div
      className={
        accent ??
        `rounded-xl border px-4 py-3 text-sm ${
          danger
            ? 'border-rose-500/30 bg-rose-500/10 text-rose-200'
            : 'border-slate-800 bg-slate-900/50 text-slate-300'
        }`
      }
    >
      {text}
    </div>
  )
}
