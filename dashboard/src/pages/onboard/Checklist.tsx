import { useCallback, useEffect, useMemo, useState } from 'react'
import type { ChecklistItem } from '../../data/onboardChecklist'
import { CHECKLIST, itemApplies } from '../../data/onboardChecklist'

interface ChecklistProps {
  machineKind: string
  osFamily: string
  targetIp: string
  targetName: string
  activeId: string | null
  onSelect: (id: string) => void
}

function storageKey(name: string) {
  return `onboard:${name}`
}

export function Checklist({
  machineKind,
  osFamily,
  targetIp,
  targetName,
  activeId,
  onSelect,
}: ChecklistProps) {
  const [checked, setChecked] = useState<Record<string, boolean>>({})
  const [verifying, setVerifying] = useState<string | null>(null)

  // Load persisted state when target name changes.
  useEffect(() => {
    try {
      const raw = localStorage.getItem(storageKey(targetName))
      setChecked(raw ? JSON.parse(raw) : {})
    } catch {
      setChecked({})
    }
  }, [targetName])

  const persist = useCallback(
    (next: Record<string, boolean>) => {
      try {
        localStorage.setItem(storageKey(targetName), JSON.stringify(next))
      } catch {
        /* noop */
      }
    },
    [targetName]
  )

  const visible = useMemo(
    () => CHECKLIST.filter((it) => itemApplies(it, machineKind, osFamily)),
    [machineKind, osFamily]
  )

  const grouped = useMemo(() => {
    const g: Record<string, ChecklistItem[]> = {}
    for (const it of visible) {
      ;(g[it.group] ||= []).push(it)
    }
    return g
  }, [visible])

  const toggle = (id: string) => {
    const next = { ...checked, [id]: !checked[id] }
    setChecked(next)
    persist(next)
  }

  const runVerify = async (it: ChecklistItem) => {
    if (!it.verify || !targetIp) return
    setVerifying(it.id)
    try {
      let ok = false
      if (it.verify.kind === 'ip_ping') {
        const r = await fetch(`/api/fleet/check-ip?ip=${encodeURIComponent(targetIp)}`)
        const d = await r.json()
        ok = !!d.reachable
      } else if (it.verify.kind === 'tcp' && it.verify.port) {
        const r = await fetch(
          `/api/fleet/check-tcp?ip=${encodeURIComponent(targetIp)}&port=${it.verify.port}`
        )
        const d = await r.json()
        ok = !!d.reachable
      }
      if (ok) {
        const next = { ...checked, [it.id]: true }
        setChecked(next)
        persist(next)
      }
    } finally {
      setVerifying(null)
    }
  }

  const totalDone = visible.filter((i) => checked[i.id]).length
  const totalItems = visible.length

  return (
    <div className="h-full overflow-auto p-4 space-y-4">
      <div className="text-xs uppercase tracking-wide text-slate-400">
        Pre-flight checklist — {totalDone}/{totalItems} done
      </div>
      {Object.entries(grouped).map(([group, items]) => (
        <div key={group} className="space-y-1">
          <div className="text-[11px] font-semibold uppercase tracking-wider text-slate-500 pl-1">
            {group}
          </div>
          {items.map((it) => {
            const isActive = activeId === it.id
            const isChecked = !!checked[it.id]
            return (
              <div
                key={it.id}
                onClick={() => onSelect(it.id)}
                className={`flex items-start gap-2 rounded px-2 py-1.5 cursor-pointer text-sm select-none ${
                  isActive ? 'bg-indigo-500/20 text-white' : 'hover:bg-slate-800 text-slate-200'
                }`}
              >
                <input
                  type="checkbox"
                  checked={isChecked}
                  onChange={() => toggle(it.id)}
                  onClick={(e) => e.stopPropagation()}
                  className="mt-1 accent-emerald-500"
                />
                <div className="flex-1">
                  <div className={isChecked ? 'line-through text-slate-400' : ''}>
                    {it.title}
                  </div>
                </div>
                {it.verify && it.verify.kind !== 'manual' && (
                  <button
                    onClick={(e) => {
                      e.stopPropagation()
                      runVerify(it)
                    }}
                    disabled={verifying === it.id || !targetIp}
                    className="text-[11px] px-1.5 py-0.5 rounded border border-slate-700 text-slate-300 hover:bg-slate-700 disabled:opacity-40"
                  >
                    {verifying === it.id ? '…' : 'Verify'}
                  </button>
                )}
              </div>
            )
          })}
        </div>
      ))}
    </div>
  )
}
