import { useCallback, useEffect, useMemo, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { cn } from '../lib/utils'

type Skill = {
  id: string
  scope: string
  name: string
  description: string
  when_to_invoke: string
  family: string
  source: string
  version: string
  tools: string[]
  triggers: string[]
}

type SkillDetail = {
  id: string
  scope: string
  name: string
  content: string
}

export function Skills() {
  const [skills, setSkills] = useState<Skill[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [detail, setDetail] = useState<SkillDetail | null>(null)
  const [detailLoading, setDetailLoading] = useState(false)
  const [query, setQuery] = useState('')

  const load = useCallback(async () => {
    try {
      setError(null)
      const res = await fetch('/api/skills')
      const payload = await res.json()
      setSkills(Array.isArray(payload.skills) ? (payload.skills as Skill[]) : [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load skills')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  useEffect(() => {
    if (!selectedId) {
      setDetail(null)
      return
    }
    let cancelled = false
    setDetailLoading(true)
    fetch(`/api/skills/${encodeURIComponent(selectedId)}`)
      .then((r) => r.json() as Promise<SkillDetail>)
      .then((data) => {
        if (!cancelled) setDetail(data)
      })
      .catch(() => {
        if (!cancelled) setDetail(null)
      })
      .finally(() => {
        if (!cancelled) setDetailLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [selectedId])

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase()
    if (!q) return skills
    return skills.filter(
      (s) =>
        s.name.toLowerCase().includes(q) ||
        s.description.toLowerCase().includes(q) ||
        s.family.toLowerCase().includes(q) ||
        s.scope.toLowerCase().includes(q) ||
        s.tools.some((t) => t.toLowerCase().includes(q)) ||
        s.triggers.some((t) => t.toLowerCase().includes(q)),
    )
  }, [skills, query])

  const scopes = useMemo(() => {
    const set = new Set(skills.map((s) => s.scope))
    return [...set].sort()
  }, [skills])

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">Skills</h1>
            {loading ? <Badge variant="info">loading</Badge> : <Badge variant="ok">live</Badge>}
          </div>
          <p className="mt-1 text-sm text-muted">
            Loadable SKILL.md capabilities discovered from the <code className="rounded bg-elevated px-1 font-mono text-primary">skills/</code> directory.
          </p>
        </div>
        <Button onClick={() => void load()} disabled={loading} type="button" variant="outline">
          Refresh
        </Button>
      </div>

      <div className="grid gap-3 sm:grid-cols-3">
        <SummaryCard label="Skills" value={skills.length.toLocaleString()} detail="discovered SKILL.md files" />
        <SummaryCard label="Scopes" value={scopes.length.toLocaleString()} detail="top-level directories" />
        <SummaryCard label="Filtered" value={filtered.length.toLocaleString()} detail="matching search" />
      </div>

      {error ? (
        <Card className="border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">{error}</Card>
      ) : null}

      <Card className="bg-panel">
        <CardHeader>
          <div>
            <CardTitle>Skill Catalog</CardTitle>
            <CardDescription>Click a skill to read the full SKILL.md.</CardDescription>
          </div>
        </CardHeader>
        <input
          aria-label="Search skills"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search skills..."
          className={fieldClass}
        />
        {loading && skills.length === 0 ? (
          <p className="mt-3 text-sm text-dim">Loading skills...</p>
        ) : filtered.length === 0 ? (
          <p className="mt-3 text-sm text-dim">No skills match.</p>
        ) : (
          <div className="mt-3 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {filtered.map((s) => (
              <button
                key={s.id}
                type="button"
                onClick={() => setSelectedId(s.id)}
                className={cn(
                  'rounded-lg border p-3 text-left transition',
                  selectedId === s.id
                    ? 'border-primary bg-primary-subtle'
                    : 'border-border bg-surface hover:border-border-subtle hover:bg-elevated',
                )}
              >
                <div className="flex items-start justify-between gap-2">
                  <p className="font-medium text-foreground">{s.name}</p>
                  <Badge variant="default">{s.scope}</Badge>
                </div>
                <p className="mt-1 line-clamp-3 text-xs text-muted">{s.description}</p>
                <div className="mt-2 flex flex-wrap gap-1">
                  {s.family ? <Badge variant="neutral">{s.family}</Badge> : null}
                  {s.tools.slice(0, 2).map((t) => (
                    <Badge key={t} variant="neutral">
                      {t}
                    </Badge>
                  ))}
                  {s.tools.length > 2 ? <Badge variant="neutral">+{s.tools.length - 2}</Badge> : null}
                </div>
              </button>
            ))}
          </div>
        )}
      </Card>

      {selectedId && (
        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>{detail?.name ?? selectedId}</CardTitle>
              <CardDescription>{detail?.id ?? selectedId}</CardDescription>
            </div>
            {detail?.scope ? <Badge variant="default">{detail.scope}</Badge> : null}
          </CardHeader>
          {detailLoading && !detail ? (
            <p className="text-sm text-dim">Loading skill content...</p>
          ) : detail ? (
            <pre className="max-h-[640px] overflow-auto whitespace-pre-wrap rounded-lg border border-border bg-background p-4 font-mono text-sm leading-6 text-foreground">
              {detail.content}
            </pre>
          ) : (
            <p className="text-sm text-dim">Unable to load skill content.</p>
          )}
        </Card>
      )}
    </section>
  )
}

function SummaryCard({ label, value, detail }: { label: string; value: string; detail: string }) {
  return (
    <Card className="bg-panel px-4 py-3">
      <CardDescription className="uppercase tracking-wide">{label}</CardDescription>
      <div className="mt-1 text-2xl font-semibold text-foreground">{value}</div>
      <p className="mt-1 text-xs text-dim">{detail}</p>
    </Card>
  )
}

const fieldClass =
  'min-h-9 w-full rounded-lg border border-border bg-elevated px-3 py-2 text-sm text-foreground outline-none transition placeholder:text-dim focus:border-primary disabled:cursor-not-allowed disabled:opacity-60'
