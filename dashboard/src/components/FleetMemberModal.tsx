import { useEffect } from 'react'
import type { FleetNode } from '../types'

type Props = {
  member: FleetNode | null
  onClose: () => void
}

export function FleetMemberModal({ member, onClose }: Props) {
  // Close on Escape
  useEffect(() => {
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [onClose])

  if (!member) return null

  const status = (member.status ?? member.health ?? 'unknown').toLowerCase()
  const isLeader = member.is_leader || member.leader_state === 'leader'
  const models = member.models ?? []
  const modelsLoaded = member.models_loaded ?? models.map(m => m.name)

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center" onClick={onClose}>
      <div className="fixed inset-0 bg-black/60" />
      <div
        className="relative w-full max-w-2xl max-h-[85vh] overflow-y-auto rounded-xl border border-zinc-700 bg-zinc-900 shadow-2xl shadow-black/50 m-4"
        onClick={e => e.stopPropagation()}
      >
        {/* Header */}
        <div className="sticky top-0 z-10 flex items-center justify-between border-b border-zinc-800 bg-zinc-900 px-6 py-4">
          <div>
            <h2 className="text-lg font-semibold text-zinc-100 capitalize">{member.name}</h2>
            <p className="text-sm text-zinc-500">{member.ip ?? 'unknown IP'} &middot; {member.role ?? 'worker'}</p>
          </div>
          <div className="flex items-center gap-2">
            <span className={`rounded-full px-2 py-0.5 text-xs font-medium ${
              status === 'online' ? 'bg-emerald-500/20 text-emerald-300' :
              status === 'degraded' ? 'bg-amber-500/20 text-amber-300' :
              'bg-rose-500/20 text-rose-300'
            }`}>{status}</span>
            {isLeader && <span className="rounded-full bg-violet-500/20 px-2 py-0.5 text-xs text-violet-300">leader</span>}
            <button onClick={onClose} className="ml-2 rounded p-1 text-zinc-500 hover:bg-zinc-800 hover:text-zinc-300">
              <svg className="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
              </svg>
            </button>
          </div>
        </div>

        <div className="space-y-4 p-6">
          {/* Hardware */}
          <Section title="Hardware">
            <Row label="CPU" value={member.cpu ?? member.hardware?.cpu} />
            <Row label="RAM" value={member.ram ?? member.hardware?.ram} />
            <Row label="GPU" value={member.gpu ?? member.hardware?.gpu} />
            <Row label="Service Version" value={member.service_version} />
          </Section>

          {/* Models */}
          <Section title="Models Running">
            {modelsLoaded.length > 0 ? (
              <div className="space-y-2">
                {modelsLoaded.map(name => {
                  const modelInfo = models.find(m => m.name === name)
                  return (
                    <div key={name} className="flex items-center justify-between rounded-lg border border-zinc-800 bg-zinc-950/50 px-3 py-2">
                      <div>
                        <p className="text-sm font-medium text-zinc-200">{name}</p>
                        {modelInfo?.endpoint && <p className="text-xs text-zinc-500">{modelInfo.endpoint}</p>}
                      </div>
                      <div className="flex items-center gap-2">
                        {modelInfo?.contextWindow && (
                          <span className="text-xs text-zinc-500">{Math.round(modelInfo.contextWindow / 1024)}K ctx</span>
                        )}
                        <span className={`h-2 w-2 rounded-full ${modelInfo?.status === 'online' ? 'bg-emerald-400' : 'bg-zinc-600'}`} />
                      </div>
                    </div>
                  )
                })}
              </div>
            ) : (
              <p className="text-sm text-zinc-500">No models reported</p>
            )}
          </Section>

          {/* Connection */}
          <Section title="Connection">
            <Row label="Last Heartbeat" value={member.last_heartbeat} />
            <Row
              label="Freshness"
              value={member.heartbeat_age_seconds != null
                ? `${member.heartbeat_freshness ?? 'unknown'} (${member.heartbeat_age_seconds}s)`
                : member.heartbeat_freshness}
            />
          </Section>

          {/* Workload */}
          {member.current_workload && (
            <Section title="Current Workload">
              <Row label="Status" value={member.current_workload.status} />
              <Row label="Active Tasks" value={member.current_workload.active_tasks ?? 0} />
              {member.current_workload.task_ids && member.current_workload.task_ids.length > 0 && (
                <Row label="Task IDs" value={member.current_workload.task_ids.join(', ')} />
              )}
            </Section>
          )}

          {/* Raw Metrics */}
          {member.metrics && Object.keys(member.metrics).length > 0 && (
            <Section title="Metrics">
              <pre className="max-h-40 overflow-auto rounded-md bg-zinc-950/80 p-3 text-xs text-zinc-300">
                {JSON.stringify(member.metrics, null, 2)}
              </pre>
            </Section>
          )}
        </div>
      </div>
    </div>
  )
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="rounded-lg border border-zinc-800 bg-zinc-900/50 p-4">
      <h3 className="mb-3 text-xs font-semibold uppercase tracking-wider text-zinc-500">{title}</h3>
      <div className="space-y-1">{children}</div>
    </div>
  )
}

function Row({ label, value }: { label: string; value: unknown }) {
  const display = value == null || value === '' ? 'unknown' : String(value)
  return (
    <div className="flex items-start justify-between gap-3 text-sm">
      <span className="text-zinc-500">{label}</span>
      <span className="text-right text-zinc-200">{display}</span>
    </div>
  )
}
