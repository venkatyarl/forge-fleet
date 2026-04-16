import { useCallback, useEffect, useMemo, useState } from 'react'
import { detectClient } from '../lib/browserDetect'
import type { DetectedClient, MachineKind } from '../lib/browserDetect'
import { Checklist } from './onboard/Checklist'
import { DetailPanel } from './onboard/DetailPanel'
import { OnboardChat } from './onboard/OnboardChat'

const LEADER_HOST = window.location.hostname || '192.168.5.100'
const LEADER_PORT = window.location.port || '51002'
const MACHINE_KINDS: { value: MachineKind; label: string }[] = [
  { value: 'apple-silicon', label: 'Apple Silicon Mac (mlx)' },
  { value: 'intel-mac', label: 'Intel Mac (llama.cpp)' },
  { value: 'linux', label: 'Linux x86 CPU (llama.cpp)' },
  { value: 'linux-gpu', label: 'Linux + NVIDIA GPU (vllm)' },
  { value: 'dgx-os', label: 'DGX OS (vllm)' },
]

function runtimeFor(kind: MachineKind): string {
  switch (kind) {
    case 'apple-silicon':
      return 'mlx'
    case 'dgx-os':
    case 'linux-gpu':
      return 'vllm'
    default:
      return 'llama.cpp'
  }
}

function osFamilyFor(kind: MachineKind): string {
  if (kind === 'apple-silicon' || kind === 'intel-mac') return 'mac'
  return 'linux'
}

export function OperatorOnboarding() {
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
  const [token, setToken] = useState<string>('')

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

  // Try to read the enrollment token from the runtime settings endpoint
  // so the copy-paste command works without the operator typing a token.
  useEffect(() => {
    fetch('/api/settings/runtime')
      .then((r) => (r.ok ? r.json() : null))
      .then((d) => {
        const s = d?.enrollment?.token?.value || d?.enrollment?.shared_secret
        if (typeof s === 'string' && s.length > 0) setToken(s)
      })
      .catch(() => {
        /* noop */
      })
  }, [])

  // Subscribe to /ws for enrollment-progress events (best-effort).
  useEffect(() => {
    if (!name) return
    let ws: WebSocket | null = null
    try {
      ws = new WebSocket(`ws://${LEADER_HOST}:${LEADER_PORT}/ws`)
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

  const curlCommand = useMemo(() => {
    const params = new URLSearchParams({
      token: token || '<SET-TOKEN-FIRST>',
      name,
      ip: ip || 'auto',
      ssh_user: sshUser,
      role,
      runtime,
    })
    return `curl -fsSL 'http://${LEADER_HOST}:${LEADER_PORT}/onboard/bootstrap.sh?${params.toString()}' | sudo bash`
  }, [token, name, ip, sshUser, role, runtime])

  const copy = useCallback(() => {
    navigator.clipboard.writeText(curlCommand).catch(() => {
      /* noop */
    })
  }, [curlCommand])

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
    <div className="h-full flex flex-col bg-slate-950 text-slate-100">
      {/* Detection card + form + copy-paste command */}
      <div className="p-4 border-b border-slate-800 space-y-3">
        <div className="flex items-start gap-6 flex-wrap">
          <div className="flex-1 min-w-[320px]">
            <div className="text-[11px] uppercase tracking-wider text-slate-500 mb-1">
              Detected this computer
            </div>
            {detected ? (
              <div className="text-sm text-slate-200 font-mono space-y-0.5">
                <div>OS family: {detected.os_family}</div>
                <div>Cores: {detected.cores}</div>
                <div>RAM hint: {detected.ram_gb_hint || '?'} GB</div>
                <div>LAN IP: {detected.lan_ip || '(WebRTC blocked)'}</div>
                <div>
                  GPU hint: <span className="text-slate-400">{detected.webgl_renderer || '—'}</span>
                </div>
                <div>Timezone: {detected.timezone}</div>
              </div>
            ) : (
              <div className="text-sm text-slate-500">Detecting…</div>
            )}
          </div>
          <div className="flex-1 min-w-[320px] grid grid-cols-2 gap-2 text-sm">
            <label className="text-slate-400 self-center">Name</label>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              className="bg-slate-900 border border-slate-700 rounded px-2 py-1"
            />
            <label className="text-slate-400 self-center">IP</label>
            <input
              value={ip}
              onChange={(e) => setIp(e.target.value)}
              placeholder="auto"
              className="bg-slate-900 border border-slate-700 rounded px-2 py-1"
            />
            <label className="text-slate-400 self-center">SSH user</label>
            <input
              value={sshUser}
              onChange={(e) => setSshUser(e.target.value)}
              className="bg-slate-900 border border-slate-700 rounded px-2 py-1"
            />
            <label className="text-slate-400 self-center">Role</label>
            <select
              value={role}
              onChange={(e) => setRole(e.target.value as typeof role)}
              className="bg-slate-900 border border-slate-700 rounded px-2 py-1"
            >
              <option value="builder">builder</option>
              <option value="gateway">gateway</option>
              <option value="testbed">testbed</option>
            </select>
            <label className="text-slate-400 self-center">Machine kind</label>
            <select
              value={machineKind}
              onChange={(e) => setMachineKind(e.target.value as MachineKind)}
              className="bg-slate-900 border border-slate-700 rounded px-2 py-1"
            >
              {MACHINE_KINDS.map((k) => (
                <option key={k.value} value={k.value}>
                  {k.label}
                </option>
              ))}
            </select>
          </div>
        </div>
        <div className="bg-slate-900 border border-indigo-500/30 rounded p-3">
          <div className="flex items-center justify-between mb-1">
            <div className="text-[11px] uppercase tracking-wider text-indigo-300">
              Copy-paste on the new computer
            </div>
            <button
              onClick={copy}
              className="text-xs px-2 py-1 rounded border border-indigo-400 text-indigo-200 hover:bg-indigo-500/20"
            >
              Copy
            </button>
          </div>
          <pre className="text-[12px] font-mono text-emerald-200 whitespace-pre-wrap break-all">
            {curlCommand}
          </pre>
          {token === '' && (
            <div className="mt-2 text-[11px] text-amber-400">
              Warning: enrollment token not yet set. Run{' '}
              <code className="bg-slate-800 px-1 rounded">
                ff secrets set enrollment.shared_secret &lt;token&gt;
              </code>{' '}
              on Taylor before running the command above.
            </div>
          )}
        </div>
      </div>

      {/* 3-column body */}
      <div className="flex-1 grid grid-cols-12 min-h-0">
        <div className="col-span-4 border-r border-slate-800 overflow-hidden">
          <Checklist
            machineKind={machineKind}
            osFamily={osFamily}
            targetIp={ip}
            targetName={name}
            activeId={activeId}
            onSelect={setActiveId}
          />
        </div>
        <div className="col-span-5 border-r border-slate-800 overflow-hidden">
          <DetailPanel activeId={activeId} />
        </div>
        <div className="col-span-3 overflow-hidden">
          <OnboardChat nodeName={name} osFamily={osFamily} machineKind={machineKind} />
        </div>
      </div>

      {/* Progress + verify */}
      <div className="border-t border-slate-800 p-3 space-y-2">
        <div className="flex items-center justify-between">
          <div className="text-[11px] uppercase tracking-wider text-slate-500">
            Enrollment progress
          </div>
          <button
            onClick={runFullVerify}
            className="text-xs px-2 py-1 rounded border border-emerald-400 text-emerald-200 hover:bg-emerald-500/20"
          >
            Run full verify
          </button>
        </div>
        {progress.length === 0 ? (
          <div className="text-sm text-slate-500">
            Waiting for enrollment-progress events… run the copy-paste command on the new machine.
          </div>
        ) : (
          <div className="space-y-0.5 font-mono text-xs max-h-40 overflow-auto">
            {progress.slice(-20).map((ev, i) => {
              const color =
                ev.status === 'ok'
                  ? 'text-emerald-300'
                  : ev.status === 'failed'
                    ? 'text-rose-300'
                    : 'text-amber-300'
              return (
                <div key={i} className={color}>
                  [{ev.at.split('T')[1]?.slice(0, 8) || '--'}] {ev.status.padEnd(8)} {ev.step}
                  {ev.detail ? ` — ${ev.detail}` : ''}
                </div>
              )
            })}
          </div>
        )}
      </div>
    </div>
  )
}
