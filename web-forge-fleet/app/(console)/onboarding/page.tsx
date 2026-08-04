'use client'

import { useCallback, useEffect, useState } from 'react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { StatusBadge } from '@/components/ui/status-badge'
import { detectClient } from '@/lib/browserDetect'
import type { DetectedClient, MachineKind } from '@/lib/browserDetect'
import { cn } from '@/lib/utils'
import { Checklist } from './_components/Checklist'
import { DetailPanel } from './_components/DetailPanel'
import { OnboardChat } from './_components/OnboardChat'

// Computed inside the component (not at module scope) so the static-export
// prerender, where `window` is undefined, does not crash.
function leaderHost(): string {
  return typeof window !== 'undefined' ? window.location.hostname || '192.168.5.100' : '192.168.5.100'
}
function leaderPort(): string {
  return typeof window !== 'undefined' ? window.location.port || '51002' : '51002'
}

const MACHINE_KINDS: { value: MachineKind; label: string }[] = [
  { value: 'apple-silicon', label: 'Apple Silicon Mac (mlx)' },
  { value: 'intel-mac', label: 'Intel Mac (llama.cpp)' },
  { value: 'linux', label: 'Linux x86 CPU (llama.cpp)' },
  { value: 'linux-gpu', label: 'Linux + NVIDIA GPU (vllm)' },
  { value: 'dgx-os', label: 'DGX OS (vllm)' },
  { value: 'windows', label: 'Windows (llama.cpp)' },
  { value: 'windows-gpu', label: 'Windows + NVIDIA GPU (vllm)' },
]

const fieldClass =
  'min-h-9 w-full rounded-lg border border-border bg-elevated px-3 py-2 text-sm text-foreground outline-hidden transition placeholder:text-dim focus:border-primary disabled:cursor-not-allowed disabled:opacity-60'
const labelClass = 'text-xs font-medium uppercase tracking-wide text-dim'
const ONBOARDING_TRANSPORT_QUARANTINE =
  'New-node onboarding is quarantined until the gateway has server-verified TLS transport. No bootstrap command was generated.'

function runtimeFor(kind: MachineKind): string {
  switch (kind) {
    case 'apple-silicon':
      return 'mlx'
    case 'dgx-os':
    case 'linux-gpu':
    case 'windows-gpu':
      return 'vllm'
    default:
      return 'llama.cpp'
  }
}

function osFamilyFor(kind: MachineKind): string {
  if (kind === 'apple-silicon' || kind === 'intel-mac') return 'mac'
  if (kind === 'windows' || kind === 'windows-gpu') return 'windows'
  return 'linux'
}

export default function OperatorOnboardingPage() {
  const [detected, setDetected] = useState<DetectedClient | null>(null)
  const [name, setName] = useState('newbox')
  const [ip, setIp] = useState('')
  const [sshUser, setSshUser] = useState('newbox')
  const [role, setRole] = useState<'builder' | 'gateway' | 'testbed'>('builder')
  const [machineKind, setMachineKind] = useState<MachineKind>('linux')
  const [activeId, setActiveId] = useState<string | null>(null)
  const [progress, setProgress] = useState<
    Array<{ step: string; status: string; detail?: string; at: string }>
  >([])

  // Kick detection once.
  useEffect(() => {
    detectClient().then((d) => {
      setDetected(d)
      setMachineKind(d.suggested_kind === 'unknown' ? 'linux' : d.suggested_kind)
      if (d.lan_ip) setIp(d.lan_ip)
    })
  }, [])

  // Keep ssh_user synced with name by default.
  useEffect(() => {
    if (sshUser === '' || sshUser === 'newbox') {
      setSshUser(name || 'newbox')
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [name])

  // Subscribe to /ws for enrollment-progress events (best-effort).
  useEffect(() => {
    if (!name) return
    let ws: WebSocket | null = null
    try {
      ws = new WebSocket(`ws://${leaderHost()}:${leaderPort()}/ws`)
      ws.onmessage = (evt) => {
        try {
          const m = JSON.parse(evt.data)
          if (m?.channel === `fleet:enrollment:${name}` || m?.name === name) {
            setProgress((prev) => [
              ...prev,
              {
                step: m.step || m.payload?.step || '?',
                status: m.status || m.payload?.status || '?',
                detail: m.detail || m.payload?.detail,
                at: m.at || m.payload?.at || new Date().toISOString(),
              },
            ])
          }
        } catch {
          /* noop */
        }
      }
    } catch {
      /* noop */
    }
    return () => {
      try {
        ws?.close()
      } catch {
        /* noop */
      }
    }
  }, [name])

  const runtime = runtimeFor(machineKind)
  const osFamily = osFamilyFor(machineKind)

  const runFullVerify = useCallback(async () => {
    setProgress((p) => [...p, { step: 'verify', status: 'running', at: new Date().toISOString() }])
    try {
      const r = await fetch(`/api/fleet/verify-node?name=${encodeURIComponent(name)}`, {
        method: 'POST',
      })
      const d = await r.json()
      setProgress((p) => [
        ...p,
        {
          step: 'verify',
          status: d.failed === 0 ? 'ok' : 'failed',
          detail: `${d.passed} pass / ${d.failed} fail / ${d.skipped} skip`,
          at: new Date().toISOString(),
        },
      ])
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e)
      setProgress((p) => [
        ...p,
        { step: 'verify', status: 'failed', detail: msg, at: new Date().toISOString() },
      ])
    }
  }, [name])

  return (
    <section className="flex h-full min-h-0 flex-col bg-background text-foreground">
      <div className="border-b border-border bg-surface p-4">
        <div className="grid gap-4 xl:grid-cols-[minmax(280px,0.9fr)_minmax(420px,1.15fr)]">
          <Card className="bg-panel">
            <CardHeader className="items-start gap-3">
              <div>
                <CardTitle>Detected Computer</CardTitle>
                <CardDescription>Browser-reported hints for this workstation.</CardDescription>
              </div>
              <StatusBadge status={detected ? 'ready' : 'pending'}>
                {detected ? 'ready' : 'detecting'}
              </StatusBadge>
            </CardHeader>
            {detected ? (
              <dl className="grid grid-cols-2 gap-x-4 gap-y-2 text-sm">
                <dt className="text-dim">OS family</dt>
                <dd className="font-mono text-foreground">{detected.os_family}</dd>
                <dt className="text-dim">Cores</dt>
                <dd className="font-mono text-foreground">{detected.cores}</dd>
                <dt className="text-dim">RAM hint</dt>
                <dd className="font-mono text-foreground">{detected.ram_gb_hint || '?'} GB</dd>
                <dt className="text-dim">LAN IP</dt>
                <dd className="font-mono text-foreground">{detected.lan_ip || '(WebRTC blocked)'}</dd>
                <dt className="text-dim">GPU hint</dt>
                <dd className="min-w-0 truncate font-mono text-muted">
                  {detected.webgl_renderer || '-'}
                </dd>
                <dt className="text-dim">Timezone</dt>
                <dd className="font-mono text-foreground">{detected.timezone}</dd>
              </dl>
            ) : (
              <div className="text-sm text-muted">Detecting...</div>
            )}
          </Card>

          <Card className="bg-panel">
            <CardHeader className="items-start gap-3">
              <div>
                <CardTitle>Node Profile</CardTitle>
                <CardDescription>
                  Staged metadata for verification; no bootstrap is generated while TLS is
                  quarantined.
                </CardDescription>
              </div>
              <Badge variant="default">{runtime}</Badge>
            </CardHeader>
            <div className="grid gap-3 sm:grid-cols-2">
              <label className="space-y-1.5">
                <span className={labelClass}>Name</span>
                <input value={name} onChange={(e) => setName(e.target.value)} className={fieldClass} />
              </label>
              <label className="space-y-1.5">
                <span className={labelClass}>IP</span>
                <input
                  value={ip}
                  onChange={(e) => setIp(e.target.value)}
                  placeholder="auto"
                  className={fieldClass}
                />
              </label>
              <label className="space-y-1.5">
                <span className={labelClass}>SSH user</span>
                <input
                  value={sshUser}
                  onChange={(e) => setSshUser(e.target.value)}
                  className={fieldClass}
                />
              </label>
              <label className="space-y-1.5">
                <span className={labelClass}>Role</span>
                <select
                  value={role}
                  onChange={(e) => setRole(e.target.value as typeof role)}
                  className={fieldClass}
                >
                  <option value="builder">builder</option>
                  <option value="gateway">gateway</option>
                  <option value="testbed">testbed</option>
                </select>
              </label>
              <label className="space-y-1.5 sm:col-span-2">
                <span className={labelClass}>Machine kind</span>
                <select
                  value={machineKind}
                  onChange={(e) => setMachineKind(e.target.value as MachineKind)}
                  className={fieldClass}
                >
                  {MACHINE_KINDS.map((k) => (
                    <option key={k.value} value={k.value}>
                      {k.label}
                    </option>
                  ))}
                </select>
              </label>
            </div>
          </Card>
        </div>

        <Card className="mt-4 bg-panel">
          <CardHeader className="items-start gap-3">
            <div>
              <CardTitle>Bootstrap unavailable</CardTitle>
              <CardDescription>
                Enrollment remains fail-closed until the dedicated HTTPS listener and certificate
                pinning are installed.
              </CardDescription>
            </div>
            <StatusBadge status="blocked">quarantined</StatusBadge>
          </CardHeader>
          <div className="rounded-lg border border-status-warn/40 bg-primary-subtle px-3 py-2 text-sm text-status-warn">
            {ONBOARDING_TRANSPORT_QUARANTINE}
          </div>
        </Card>
      </div>

      <div className="grid min-h-0 flex-1 grid-cols-1 bg-background lg:grid-cols-12">
        <div className="min-h-0 overflow-hidden border-b border-border bg-surface lg:col-span-4 lg:border-b-0 lg:border-r">
          <Checklist
            machineKind={machineKind}
            osFamily={osFamily}
            targetIp={ip}
            targetName={name}
            activeId={activeId}
            onSelect={setActiveId}
          />
        </div>
        <div className="min-h-0 overflow-hidden border-b border-border bg-background lg:col-span-5 lg:border-b-0 lg:border-r">
          <DetailPanel activeId={activeId} />
        </div>
        <div className="min-h-0 overflow-hidden bg-surface lg:col-span-3">
          <OnboardChat nodeName={name} osFamily={osFamily} machineKind={machineKind} />
        </div>
      </div>

      <div className="space-y-3 border-t border-border bg-surface p-4">
        <div className="flex items-center justify-between">
          <div>
            <div className="text-xs font-medium uppercase tracking-wide text-dim">
              Enrollment progress
            </div>
            <div className="text-xs text-muted">{progress.length} recorded events</div>
          </div>
          <Button onClick={runFullVerify} variant="outline" size="sm" className="text-status-ok">
            Run full verify
          </Button>
        </div>
        {progress.length === 0 ? (
          <div className="rounded-lg border border-border bg-panel px-3 py-2 text-sm text-muted">
            Waiting for enrollment-progress events. Bootstrap remains quarantined until
            server-verified TLS is available.
          </div>
        ) : (
          <div className="max-h-40 space-y-1 overflow-auto rounded-lg border border-border bg-panel p-2 font-mono text-xs">
            {progress.slice(-20).map((ev, i) => {
              const color =
                ev.status === 'ok'
                  ? 'text-status-ok'
                  : ev.status === 'failed'
                    ? 'text-status-crit'
                    : ev.status === 'running'
                      ? 'text-status-warn'
                      : 'text-status-info'
              return (
                <div key={i} className={cn('rounded-sm px-2 py-1', color)}>
                  [{ev.at.split('T')[1]?.slice(0, 8) || '--'}] {ev.status.padEnd(8)} {ev.step}
                  {ev.detail ? ` - ${ev.detail}` : ''}
                </div>
              )
            })}
          </div>
        )}
      </div>
    </section>
  )
}
