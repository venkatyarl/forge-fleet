'use client'

import { useEffect, useRef, useState } from 'react'
import Link from 'next/link'
import { useQuery } from '@tanstack/react-query'
import {
  Activity,
  ArrowRight,
  BarChart3,
  Bot,
  Brain,
  CodeXml,
  FlaskConical,
  GitBranch,
  GraduationCap,
  Hourglass,
  KanbanSquare,
  LayoutDashboard,
  Network,
  Package,
  Server,
  Ticket,
  Users,
  Zap,
  type LucideIcon,
} from 'lucide-react'
import { getJson } from '@/lib/api'
import { extractNodes, extractSummary } from '@/lib/normalizers'
import { cn } from '@/lib/utils'
import type { FleetStatusResponse } from '@/types'

/* ------------------------------------------------------------------ data */

type OpenAiModelsResponse = { data?: { id?: string }[] }

type WorkItemLike = { status?: string }

const DONE_STATUSES = new Set(['done', 'completed', 'complete', 'closed', 'cancelled', 'canceled'])

function pickNumber(obj: Record<string, unknown>, keys: string[]): number | undefined {
  for (const key of keys) {
    const value = obj[key]
    if (typeof value === 'number' && Number.isFinite(value)) return value
  }
  return undefined
}

function useFleetSnapshot() {
  return useQuery({
    queryKey: ['landing', 'fleet'],
    refetchInterval: 20_000,
    queryFn: async () => {
      const data = await getJson<FleetStatusResponse>('/api/fleet/status').catch(() =>
        getJson<FleetStatusResponse>('/api/status')
      )
      return { nodes: extractNodes(data), summary: extractSummary(data) }
    },
  })
}

function useModelCount() {
  return useQuery({
    queryKey: ['landing', 'models'],
    refetchInterval: 30_000,
    queryFn: async () => {
      const data = await getJson<OpenAiModelsResponse>('/v1/models')
      return Array.isArray(data?.data) ? data.data.length : 0
    },
  })
}

function useProxyStats() {
  return useQuery({
    queryKey: ['landing', 'proxy-stats'],
    refetchInterval: 15_000,
    queryFn: async () => {
      const stats = await getJson<Record<string, unknown>>('/api/proxy/stats').catch(() =>
        getJson<Record<string, unknown>>('/v1/proxy/stats')
      )
      return {
        tokensPerSec: pickNumber(stats, [
          'tokens_per_sec',
          'tokensPerSec',
          'tps',
          'tokens_per_second',
        ]),
        totalRequests: pickNumber(stats, ['totalRequests', 'total_requests', 'requests']),
      }
    },
  })
}

function useActiveWorkItems() {
  return useQuery({
    queryKey: ['landing', 'work-items'],
    refetchInterval: 30_000,
    queryFn: async () => {
      const items = await getJson<WorkItemLike[]>('/api/mc/work-items').catch(() => [])
      if (!Array.isArray(items)) return 0
      return items.filter((item) => !DONE_STATUSES.has((item?.status ?? '').toLowerCase())).length
    },
  })
}

/* ------------------------------------------------------- count-up hook */

function useCountUp(target: number | undefined, duration = 900): number | undefined {
  const [value, setValue] = useState<number | undefined>(target)
  const previous = useRef<number | undefined>(target)

  useEffect(() => {
    if (target === undefined) {
      previous.current = undefined
      setValue(undefined)
      return
    }
    const from = previous.current ?? target
    previous.current = target
    if (from === target) {
      setValue(target)
      return
    }
    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
      setValue(target)
      return
    }
    let raf = 0
    const start = performance.now()
    const tick = (now: number) => {
      const t = Math.min(1, (now - start) / duration)
      const eased = 1 - Math.pow(1 - t, 3)
      setValue(Math.round(from + (target - from) * eased))
      if (t < 1) raf = requestAnimationFrame(tick)
    }
    raf = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(raf)
  }, [target, duration])

  return value
}

/* ------------------------------------------------------------- pieces */

const CAPABILITIES: { href: string; icon: LucideIcon; title: string; blurb: string }[] = [
  { href: '/fleet', icon: Server, title: 'Fleet', blurb: 'Every GPU node, its health, and what it is serving.' },
  { href: '/mission-control', icon: LayoutDashboard, title: 'Mission Control', blurb: 'The live command deck for the whole mesh.' },
  { href: '/models', icon: Zap, title: 'Models', blurb: 'Loaded models, tiers, and context windows.' },
  { href: '/model-hub', icon: Package, title: 'Model Hub', blurb: 'Catalog, downloads, and deployments across nodes.' },
  { href: '/projects', icon: KanbanSquare, title: 'PM / Work Items', blurb: 'The project board the fleet schedules work from.' },
  { href: '/brain', icon: Brain, title: 'Brain', blurb: 'Shared memory, knowledge graph, and decisions.' },
  { href: '/council', icon: Users, title: 'Council', blurb: 'Multi-LLM consensus for hard calls.' },
  { href: '/pulse', icon: Activity, title: 'Pulse', blurb: 'Real-time telemetry and fleet load at a glance.' },
  { href: '/metrics', icon: BarChart3, title: 'Metrics', blurb: 'Throughput, latency, and cost trends.' },
  { href: '/workstreams', icon: GitBranch, title: 'Workstreams', blurb: 'Which CLI session owns which lane, live.' },
  { href: '/playground', icon: FlaskConical, title: 'Playground', blurb: 'Chat with any model on the fleet, streamed.' },
  { href: '/agents', icon: Bot, title: 'Agents', blurb: 'Sub-agents, sessions, and orchestration.' },
  { href: '/cortex', icon: CodeXml, title: 'Cortex', blurb: 'The code graph: callers, callees, blast radius, tests.' },
  { href: '/training', icon: GraduationCap, title: 'Training', blurb: 'ff-LLM training jobs, loss curves, and results.' },
  { href: '/queue', icon: Hourglass, title: 'Deferred Queue', blurb: 'Tasks held back from scheduling — promote to run now.' },
  { href: '/jira', icon: Ticket, title: 'Jira Monitor', blurb: 'Watched Jira queues and live issue activity.' },
]

function Stat({ label, value, suffix }: { label: string; value: number | undefined; suffix?: string }) {
  const animated = useCountUp(value)
  return (
    <div className="flex flex-col items-center gap-1 px-6 py-5 sm:py-6">
      <span className="font-mono text-3xl font-semibold tabular-nums tracking-tight text-foreground sm:text-4xl">
        {animated === undefined ? '—' : animated.toLocaleString()}
        {suffix && animated !== undefined ? (
          <span className="ml-1 text-base font-normal text-dim">{suffix}</span>
        ) : null}
      </span>
      <span className="text-2xs font-medium tracking-widest text-dim uppercase">{label}</span>
    </div>
  )
}

/* --------------------------------------------------------------- page */

export default function LandingPage() {
  const fleet = useFleetSnapshot()
  const modelCount = useModelCount()
  const proxy = useProxyStats()
  const activeWorkItems = useActiveWorkItems()

  const summary = fleet.data?.summary
  const nodesOnline = summary?.connected_nodes ?? 0
  const fleetLive = fleet.isSuccess && nodesOnline > 0
  const version = summary?.gateway_version

  const stats = {
    nodes: fleet.isSuccess ? nodesOnline : undefined,
    models: modelCount.isSuccess ? (modelCount.data || summary?.model_count) : undefined,
    tps: proxy.isSuccess ? proxy.data.tokensPerSec : undefined,
    requests: proxy.isSuccess ? proxy.data.totalRequests : undefined,
    workItems: activeWorkItems.isSuccess ? activeWorkItems.data : undefined,
  }

  return (
    <div className="relative min-h-screen overflow-x-clip bg-background">
      {/* Ambient background: drifting violet orbs + faint grid */}
      <div aria-hidden className="pointer-events-none absolute inset-0 overflow-hidden">
        <div className="ff-grid absolute inset-0" />
        <div className="ff-orb ff-orb-a left-[-10%] top-[-15%] h-[42rem] w-[42rem] bg-primary/14" />
        <div className="ff-orb ff-orb-b right-[-12%] top-[20%] h-[36rem] w-[36rem] bg-violet-400/10" />
        <div className="ff-orb ff-orb-c bottom-[-20%] left-[25%] h-[38rem] w-[38rem] bg-primary/8" />
      </div>

      {/* Hero */}
      <section className="relative flex min-h-svh flex-col items-center justify-center px-6 text-center">
        <div
          className={cn(
            'mb-8 inline-flex items-center gap-2.5 rounded-full border border-border bg-panel/70 py-1.5 pr-4 pl-3 text-xs text-muted backdrop-blur-sm',
            fleet.isError && 'text-status-warn'
          )}
        >
          <span
            className={cn(
              'ff-pulse-dot h-2 w-2 rounded-full',
              fleetLive ? 'bg-status-ok' : fleet.isError ? 'bg-status-warn' : 'bg-dim'
            )}
          />
          {fleet.isError
            ? 'gateway unreachable'
            : fleet.isSuccess
              ? `${nodesOnline} node${nodesOnline === 1 ? '' : 's'} online`
              : 'contacting gateway…'}
        </div>

        <h1 className="text-6xl font-bold tracking-tighter text-foreground sm:text-7xl md:text-8xl">
          Forge<span className="text-primary">Fleet</span>
        </h1>
        <p className="mt-6 max-w-xl text-base leading-relaxed text-muted sm:text-lg">
          A distributed AI fleet. GPU nodes serving local LLMs, agents that orchestrate
          themselves, a code graph that knows your codebase — one command mesh.
        </p>

        <div className="mt-10 flex flex-col items-center gap-3 sm:flex-row">
          <Link
            href="/mission-control"
            className="group inline-flex h-11 items-center gap-2 rounded-xl bg-primary px-6 text-sm font-semibold text-white shadow-glow transition-colors hover:bg-primary-muted"
          >
            Enter Console
            <ArrowRight className="h-4 w-4 transition-transform group-hover:translate-x-0.5" />
          </Link>
          <Link
            href="/playground"
            className="inline-flex h-11 items-center gap-2 rounded-xl border border-border bg-panel/70 px-6 text-sm font-medium text-foreground backdrop-blur-sm transition-colors hover:bg-elevated"
          >
            <FlaskConical className="h-4 w-4 text-primary-muted" />
            Open Playground
          </Link>
        </div>
      </section>

      {/* Live stat strip */}
      <section className="relative border-y border-border bg-surface/60 backdrop-blur-sm">
        <div className="mx-auto grid max-w-5xl grid-cols-2 divide-x divide-border sm:grid-cols-4">
          <Stat label="nodes online" value={stats.nodes} />
          <Stat label="models serving" value={stats.models} />
          {stats.tps !== undefined ? (
            <Stat label="tokens / sec" value={stats.tps} />
          ) : (
            <Stat label="total requests" value={stats.requests} />
          )}
          <Stat label="active work items" value={stats.workItems} />
        </div>
      </section>

      {/* Capability grid */}
      <section className="relative mx-auto max-w-6xl px-6 py-20 sm:py-28">
        <div className="mb-12 text-center">
          <h2 className="text-2xl font-semibold tracking-tight text-foreground sm:text-3xl">
            One mesh, every surface
          </h2>
          <p className="mt-3 text-sm text-muted">
            Each console view is a live window into the same fleet.
          </p>
        </div>
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
          {CAPABILITIES.map(({ href, icon: Icon, title, blurb }) => (
            <Link
              key={href}
              href={href}
              className="group rounded-xl border border-border bg-panel p-5 transition-all hover:-translate-y-0.5 hover:border-primary/40 hover:bg-elevated hover:shadow-glow"
            >
              <div className="mb-3 inline-flex h-9 w-9 items-center justify-center rounded-lg bg-primary-subtle text-primary-muted transition-colors group-hover:text-primary">
                <Icon className="h-4.5 w-4.5" />
              </div>
              <h3 className="flex items-center gap-1.5 text-sm font-semibold text-foreground">
                {title}
                <ArrowRight className="h-3.5 w-3.5 text-dim opacity-0 transition-all group-hover:translate-x-0.5 group-hover:text-primary-muted group-hover:opacity-100" />
              </h3>
              <p className="mt-1.5 text-xs leading-relaxed text-dim">{blurb}</p>
            </Link>
          ))}
        </div>
      </section>

      {/* Footer */}
      <footer className="relative border-t border-border">
        <div className="mx-auto flex max-w-6xl flex-col items-center justify-between gap-2 px-6 py-6 text-2xs text-dim sm:flex-row">
          <span className="font-mono">{version ? `ff-gateway ${version}` : 'ff-gateway'}</span>
          <span>served by ff-gateway · ForgeFleet command mesh</span>
        </div>
      </footer>
    </div>
  )
}
