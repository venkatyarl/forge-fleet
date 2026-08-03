'use client'

import { useEffect, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  ArrowDownLeft,
  ArrowUpRight,
  CodeXml,
  FileCode,
  FlaskConical,
  RefreshCw,
  Search,
  Sparkles,
  Zap,
} from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { getJson } from '@/lib/api'
import { cn } from '@/lib/utils'

/* The cortex endpoints are served by ff-gateway; shapes are read defensively.
   /api/cortex/context currently answers {"error":"Internal error"} on some
   deployments — any payload carrying an `error` field is treated as a failure. */

type Corpus = {
  slug?: string
  title?: string
  content_nodes?: number
  sources?: number
}

type CortexHit = {
  id?: string
  qualified_name?: string
  node_type?: string
  file?: string | null
  start_line?: number | null
  fan_in?: number
  score?: number | null
}

type HitsResponse = {
  hits?: CortexHit[]
  count?: number
  semantic?: boolean
  error?: unknown
}

type ContextResponse = {
  symbol?: string
  qualified_name?: string
  kind?: string
  node_type?: string
  file?: string | null
  start_line?: number | null
  end_line?: number | null
  snippet?: string | null
  source?: string | null
  definition?: string | null
  summary?: string | null
  truncated?: boolean
  community?: { name?: string; summary?: string | null } | null
  error?: unknown
}

type ListResponse = { error?: unknown } & Record<string, unknown>

type SearchMode = 'name' | 'semantic'
type DetailTab = 'context' | 'callers' | 'callees' | 'impact' | 'tests'

const DETAIL_TABS: Array<{ id: DetailTab; label: string; icon: typeof CodeXml }> = [
  { id: 'context', label: 'Context', icon: FileCode },
  { id: 'callers', label: 'Callers', icon: ArrowDownLeft },
  { id: 'callees', label: 'Callees', icon: ArrowUpRight },
  { id: 'impact', label: 'Impact', icon: Zap },
  { id: 'tests', label: 'Tests', icon: FlaskConical },
]

async function getCortex<T extends object>(path: string): Promise<T> {
  const payload = await getJson<T & { error?: unknown }>(path)
  const err = payload && typeof payload === 'object' ? payload.error : null
  if (err) {
    const msg =
      typeof err === 'string'
        ? err
        : typeof (err as { message?: unknown })?.message === 'string'
          ? (err as { message: string }).message
          : 'cortex endpoint returned an error'
    throw new Error(msg)
  }
  return payload
}

function kindOf(hit: CortexHit): string {
  return (hit.node_type ?? 'unknown').replace(/^code:/, '')
}

function leafName(qualified?: string): string {
  if (!qualified) return 'unknown'
  const parts = qualified.split('::')
  return parts[parts.length - 1] || qualified
}

function fileLabel(hit: CortexHit): string | null {
  if (!hit.file) return null
  const short = hit.file.split('/crates/').pop() ?? hit.file.split('/src/').pop() ?? hit.file
  return hit.start_line != null ? `${short}:${hit.start_line}` : short
}

function HitRow({
  hit,
  active,
  onSelect,
}: {
  hit: CortexHit
  active?: boolean
  onSelect?: (hit: CortexHit) => void
}) {
  const file = fileLabel(hit)
  const inner = (
    <>
      <div className="flex min-w-0 items-center gap-2">
        <span className="truncate font-mono text-xs text-status-info">
          {leafName(hit.qualified_name)}
        </span>
        <Badge variant="neutral">{kindOf(hit)}</Badge>
        {typeof hit.fan_in === 'number' ? (
          <Badge variant="default" title="direct callers (fan-in)">
            fan-in {hit.fan_in}
          </Badge>
        ) : null}
      </div>
      <div className="mt-0.5 truncate font-mono text-2xs text-dim" title={hit.qualified_name ?? ''}>
        {hit.qualified_name ?? '—'}
      </div>
      {file ? (
        <div className="mt-0.5 truncate font-mono text-2xs text-dim" title={hit.file ?? ''}>
          {file}
        </div>
      ) : null}
    </>
  )

  if (!onSelect) {
    return <div className="rounded-lg border border-border bg-panel px-3 py-2">{inner}</div>
  }
  return (
    <button
      type="button"
      onClick={() => onSelect(hit)}
      className={cn(
        'w-full rounded-lg border border-border bg-panel px-3 py-2 text-left transition hover:border-border-subtle hover:bg-elevated',
        active && 'border-primary/40 bg-primary-subtle'
      )}
    >
      {inner}
    </button>
  )
}

function extractHits(payload: ListResponse | null | undefined, key: string): CortexHit[] {
  if (!payload || typeof payload !== 'object') return []
  const list = (payload as Record<string, unknown>)[key]
  return Array.isArray(list) ? (list as CortexHit[]) : []
}

function DetailPanel({ corpus, hit }: { corpus: string; hit: CortexHit }) {
  const [tab, setTab] = useState<DetailTab>('context')
  const symbol = hit.qualified_name ?? ''
  const encoded = encodeURIComponent(symbol)
  const encodedCorpus = encodeURIComponent(corpus)

  const contextQuery = useQuery({
    queryKey: ['cortex', 'context', corpus, symbol],
    enabled: tab === 'context' && symbol.length > 0,
    retry: 1,
    queryFn: () =>
      getCortex<ContextResponse>(`/api/cortex/context?corpus=${encodedCorpus}&symbol=${encoded}`),
  })

  const listQuery = useQuery({
    queryKey: ['cortex', tab, corpus, symbol],
    enabled: tab !== 'context' && symbol.length > 0,
    retry: 1,
    queryFn: () => {
      const base = `/api/cortex/${tab}?corpus=${encodedCorpus}&symbol=${encoded}`
      const url = tab === 'impact' ? `${base}&max_depth=5` : base
      return getCortex<ListResponse>(url)
    },
  })

  const listKey = tab === 'impact' ? 'impacted' : tab
  const listItems = tab !== 'context' ? extractHits(listQuery.data, listKey) : []
  const activeQuery = tab === 'context' ? contextQuery : listQuery
  const ctx = contextQuery.data

  return (
    <Card className="bg-surface p-0">
      <CardHeader className="mb-0 flex-col items-start gap-2 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <CardTitle className="truncate font-mono text-sm text-status-info">
            {hit.qualified_name ?? 'unknown symbol'}
          </CardTitle>
          <CardDescription className="mt-1">
            {kindOf(hit)} · {fileLabel(hit) ?? 'location unknown'}
          </CardDescription>
        </div>
        <Badge variant="neutral">{corpus}</Badge>
      </CardHeader>

      <nav className="flex flex-wrap gap-1 border-b border-border px-3 py-2" aria-label="Symbol detail sections">
        {DETAIL_TABS.map((t) => {
          const Icon = t.icon
          const active = t.id === tab
          return (
            <Button
              key={t.id}
              type="button"
              variant="ghost"
              size="sm"
              onClick={() => setTab(t.id)}
              aria-current={active ? 'page' : undefined}
              className={cn(
                'text-muted hover:text-foreground',
                active && 'bg-primary-subtle text-primary hover:bg-primary-subtle hover:text-primary'
              )}
            >
              <Icon className="h-3.5 w-3.5" />
              {t.label}
            </Button>
          )
        })}
      </nav>

      <div className="max-h-[32rem] space-y-2 overflow-y-auto p-4">
        {activeQuery.isLoading ? (
          <div className="space-y-2">
            {Array.from({ length: 3 }, (_, i) => (
              <div key={i} className="h-14 animate-pulse rounded-lg bg-panel/60" />
            ))}
          </div>
        ) : activeQuery.isError ? (
          <div className="rounded-xl border border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">
            {tab === 'context'
              ? 'Context unavailable: '
              : `${t(tab)} unavailable: `}
            <span className="font-mono text-xs">
              {activeQuery.error instanceof Error ? activeQuery.error.message : 'request failed'}
            </span>
          </div>
        ) : tab === 'context' ? (
          <ContextView ctx={ctx} />
        ) : listItems.length === 0 ? (
          <p className="py-6 text-center text-xs text-dim">
            No {tab === 'impact' ? 'impacted symbols' : tab} found for this symbol.
          </p>
        ) : (
          listItems.map((item, idx) => <HitRow key={item.id ?? `${item.qualified_name}-${idx}`} hit={item} />)
        )}
      </div>
    </Card>
  )
}

function t(tab: DetailTab): string {
  return DETAIL_TABS.find((x) => x.id === tab)?.label ?? tab
}

function ContextView({ ctx }: { ctx: ContextResponse | undefined }) {
  if (!ctx) return <p className="py-6 text-center text-xs text-dim">No context returned.</p>
  const code = ctx.snippet ?? ctx.source ?? ctx.definition ?? null
  const summary = ctx.community?.summary ?? ctx.summary ?? null

  if (!code && !summary) {
    return (
      <p className="py-6 text-center text-xs text-dim">
        The context endpoint returned no snippet or summary for this symbol.
      </p>
    )
  }

  return (
    <div className="space-y-3">
      {summary ? (
        <div className="rounded-lg border border-border bg-panel px-3 py-2 text-xs leading-relaxed text-muted">
          <span className="mb-1 block text-2xs font-medium uppercase tracking-wide text-dim">
            {ctx.community?.name ? `Community · ${ctx.community.name}` : 'Summary'}
          </span>
          {summary}
        </div>
      ) : null}
      {code ? (
        <pre className="overflow-x-auto rounded-lg border border-border bg-background p-3 font-mono text-2xs leading-relaxed text-foreground">
          {code}
          {ctx.truncated ? '\n… (truncated)' : ''}
        </pre>
      ) : null}
    </div>
  )
}

export default function CortexPage() {
  const [corpus, setCorpus] = useState<string | null>(null)
  const [mode, setMode] = useState<SearchMode>('name')
  const [input, setInput] = useState('')
  const [submitted, setSubmitted] = useState('')
  const [selected, setSelected] = useState<CortexHit | null>(null)

  const corporaQuery = useQuery({
    queryKey: ['cortex', 'corpora'],
    refetchInterval: 60_000,
    retry: 1,
    queryFn: async () => {
      const payload = await getCortex<{ corpora?: Corpus[] }>('/api/cortex/corpora')
      return Array.isArray(payload?.corpora) ? payload.corpora : []
    },
  })

  const corpora = corporaQuery.data ?? []

  // Default to the corpus with the most content nodes once the list arrives.
  useEffect(() => {
    if (corpus !== null || corpora.length === 0) return
    const richest = [...corpora].sort(
      (a, b) => (b.content_nodes ?? 0) - (a.content_nodes ?? 0)
    )[0]
    if (richest?.slug) setCorpus(richest.slug)
  }, [corpora, corpus])

  const searchQuery = useQuery({
    queryKey: ['cortex', 'find', corpus, mode, submitted],
    enabled: corpus !== null && submitted.trim().length > 0,
    retry: 1,
    queryFn: async () => {
      const endpoint = mode === 'semantic' ? 'search' : 'find'
      const limit = mode === 'semantic' ? 10 : 20
      const payload = await getCortex<HitsResponse>(
        `/api/cortex/${endpoint}?corpus=${encodeURIComponent(corpus ?? '')}&query=${encodeURIComponent(submitted.trim())}&limit=${limit}`
      )
      return Array.isArray(payload?.hits) ? payload.hits : []
    },
  })

  const results = searchQuery.data ?? []

  const runSearch = () => {
    setSelected(null)
    setSubmitted(input)
  }

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-xl font-semibold text-foreground">Cortex Code Graph</h2>
            {corporaQuery.isLoading ? (
              <Badge variant="info">loading</Badge>
            ) : corporaQuery.isError ? (
              <Badge variant="crit">unavailable</Badge>
            ) : (
              <Badge variant="ok">live</Badge>
            )}
          </div>
          <p className="mt-1 text-sm text-muted">
            Navigate indexed code by name or intent — callers, callees, blast radius, and tests.
          </p>
        </div>
        <Button
          onClick={() => void corporaQuery.refetch()}
          type="button"
          variant="outline"
          disabled={corporaQuery.isFetching}
        >
          <RefreshCw className={cn('h-3.5 w-3.5', corporaQuery.isFetching && 'animate-spin')} />
          Refresh
        </Button>
      </div>

      {corporaQuery.isError ? (
        <div className="rounded-xl border border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">
          Error: {corporaQuery.error instanceof Error ? corporaQuery.error.message : 'failed to load corpora'}
        </div>
      ) : null}

      <Card className="bg-surface">
        <div className="flex flex-col gap-3 lg:flex-row lg:items-center">
          <label className="flex items-center gap-2 text-xs text-dim">
            Corpus
            <select
              value={corpus ?? ''}
              onChange={(e) => {
                setCorpus(e.target.value || null)
                setSelected(null)
              }}
              className="h-8 rounded-lg border border-border bg-panel px-2 font-mono text-xs text-foreground"
            >
              {corpora.length === 0 ? <option value="">loading…</option> : null}
              {corpora.map((c) => (
                <option key={c.slug} value={c.slug}>
                  {c.title ?? c.slug} ({(c.content_nodes ?? 0).toLocaleString()} nodes)
                </option>
              ))}
            </select>
          </label>

          <form
            className="flex flex-1 items-center gap-2"
            onSubmit={(e) => {
              e.preventDefault()
              runSearch()
            }}
          >
            <div className="flex overflow-hidden rounded-lg border border-border text-2xs">
              {(['name', 'semantic'] as const).map((m) => (
                <button
                  key={m}
                  type="button"
                  onClick={() => setMode(m)}
                  className={cn(
                    'inline-flex items-center gap-1 px-2 py-1.5 font-medium transition',
                    mode === m
                      ? 'bg-primary-subtle text-primary'
                      : 'bg-panel text-dim hover:text-muted'
                  )}
                >
                  {m === 'semantic' ? <Sparkles className="h-3 w-3" /> : <Search className="h-3 w-3" />}
                  {m}
                </button>
              ))}
            </div>
            <input
              value={input}
              onChange={(e) => setInput(e.target.value)}
              placeholder={
                mode === 'semantic'
                  ? 'Describe intent, e.g. "where heartbeats are published"'
                  : 'Symbol name, e.g. load_model'
              }
              className="h-8 min-w-0 flex-1 rounded-lg border border-border bg-panel px-3 text-sm text-foreground placeholder:text-dim"
            />
            <Button type="submit" disabled={corpus === null || input.trim().length === 0}>
              Search
            </Button>
          </form>
        </div>
      </Card>

      <div className="grid gap-4 lg:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
        <Card className="bg-surface p-0">
          <CardHeader className="mb-0 border-b border-border px-4 py-3">
            <div>
              <CardTitle>Results</CardTitle>
              <CardDescription>
                {submitted
                  ? `${results.length} hit${results.length === 1 ? '' : 's'} for “${submitted}” (${mode} mode)`
                  : 'Run a search to list symbols.'}
              </CardDescription>
            </div>
            {searchQuery.isFetching ? <Badge variant="info">searching</Badge> : null}
          </CardHeader>
          <div className="max-h-[32rem] space-y-2 overflow-y-auto p-3">
            {searchQuery.isLoading ? (
              Array.from({ length: 4 }, (_, i) => (
                <div key={i} className="h-14 animate-pulse rounded-lg bg-panel/60" />
              ))
            ) : searchQuery.isError ? (
              <div className="rounded-xl border border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">
                Error: {searchQuery.error instanceof Error ? searchQuery.error.message : 'search failed'}
              </div>
            ) : !submitted ? (
              <p className="py-8 text-center text-xs text-dim">
                Pick a corpus, type a symbol name or a natural-language intent, and search.
              </p>
            ) : results.length === 0 ? (
              <p className="py-8 text-center text-xs text-dim">No symbols matched.</p>
            ) : (
              results.map((hit, idx) => (
                <HitRow
                  key={hit.id ?? `${hit.qualified_name}-${idx}`}
                  hit={hit}
                  active={selected?.id != null && selected.id === hit.id}
                  onSelect={setSelected}
                />
              ))
            )}
          </div>
        </Card>

        {selected ? (
          <DetailPanel corpus={corpus ?? ''} hit={selected} />
        ) : (
          <Card className="flex flex-col items-center justify-center gap-3 bg-surface py-16 text-center">
            <CodeXml className="h-8 w-8 text-dim" />
            <p className="text-sm font-medium text-foreground">No symbol selected</p>
            <p className="max-w-md text-xs text-dim">
              Click a search result to inspect its context, callers, callees, transitive impact,
              and covering tests.
            </p>
          </Card>
        )}
      </div>
    </section>
  )
}
