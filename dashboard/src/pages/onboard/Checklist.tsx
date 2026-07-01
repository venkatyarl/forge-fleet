import { useCallback, useEffect, useMemo, useState } from 'react'
import { Badge } from '../../components/ui/badge'
import { Button } from '../../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../../components/ui/card'
import type { ChecklistItem } from '../../data/onboardChecklist'
import { CHECKLIST, itemApplies } from '../../data/onboardChecklist'
import { cn } from '../../lib/utils'

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
    <div className="h-full space-y-4 overflow-auto p-4">
      <Card className="bg-panel">
        <CardHeader className="mb-0 items-start gap-3">
          <div>
            <CardTitle>Pre-flight Checklist</CardTitle>
            <CardDescription>
              {totalDone}/{totalItems} steps complete
            </CardDescription>
          </div>
          <Badge variant={totalDone === totalItems && totalItems > 0 ? 'ok' : 'info'}>
            {totalDone}/{totalItems}
          </Badge>
        </CardHeader>
      </Card>
      {Object.entries(grouped).map(([group, items]) => (
        <section key={group} className="space-y-2">
          <div className="px-1 text-xs font-semibold uppercase tracking-wide text-dim">{group}</div>
          {items.map((it) => {
            const isActive = activeId === it.id
            const isChecked = !!checked[it.id]
            return (
              <div
                key={it.id}
                onClick={() => onSelect(it.id)}
                role="button"
                tabIndex={0}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault()
                    onSelect(it.id)
                  }
                }}
                className={cn(
                  'flex w-full cursor-pointer items-start gap-3 rounded-lg border px-3 py-2 text-left text-sm transition',
                  isActive
                    ? 'border-primary bg-primary-subtle text-primary'
                    : 'border-border bg-panel text-foreground hover:border-border-subtle hover:bg-elevated'
                )}
              >
                <input
                  aria-label={`Toggle ${it.title}`}
                  type="checkbox"
                  checked={isChecked}
                  onChange={() => toggle(it.id)}
                  onClick={(e) => e.stopPropagation()}
                  className="mt-1 accent-primary"
                />
                <div className="min-w-0 flex-1">
                  <div className={cn('leading-5', isChecked && 'text-dim line-through')}>
                    {it.title}
                  </div>
                </div>
                {it.verify && it.verify.kind !== 'manual' && (
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={(e) => {
                      e.stopPropagation()
                      runVerify(it)
                    }}
                    disabled={verifying === it.id || !targetIp}
                    className="h-6 shrink-0 px-2 text-xs"
                  >
                    {verifying === it.id ? '...' : 'Verify'}
                  </Button>
                )}
              </div>
            )
          })}
        </section>
      ))}
    </div>
  )
}
