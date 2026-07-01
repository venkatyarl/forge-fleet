import { useEffect, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'
import { cn } from '../lib/utils'

type ModelStats = {
  model: string
  request_count: number
  total_prompt_tokens: number
  total_completion_tokens: number
  total_tokens: number
  total_cost_usd: number
  local_request_count: number
  cloud_request_count: number
  cloud_cost_usd: number
  avg_latency_ms: number
}

type FleetSummary = {
  total_requests: number
  total_prompt_tokens: number
  total_completion_tokens: number
  total_tokens: number
  total_cost_usd: number
  local_requests: number
  cloud_requests: number
  cloud_cost_usd: number
  savings_vs_cloud_only_usd: number
  models: ModelStats[]
  daily_cost_usd: number
  daily_budget_usd: number
  budget_remaining_usd: number
  budget_percent_used: number
}

type BudgetConfig = {
  daily_budget_usd: number
  cloud_daily_budget_usd: number
  enforce_budget: boolean
  alert_threshold: number
}

export function CostLedger() {
  const [summary, setSummary] = useState<FleetSummary | null>(null)
  const [budget, setBudget] = useState<BudgetConfig | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    const fetchData = async () => {
      try {
        const [summaryRes, budgetRes] = await Promise.all([
          fetch('/api/ledger/summary'),
          fetch('/api/ledger/budget'),
        ])
        if (!summaryRes.ok) throw new Error('Failed to load summary')
        if (!budgetRes.ok) throw new Error('Failed to load budget')
        setSummary(await summaryRes.json())
        setBudget(await budgetRes.json())
      } catch (e) {
        setError(e instanceof Error ? e.message : 'Failed to load cost ledger')
      } finally {
        setLoading(false)
      }
    }
    fetchData()
    const interval = setInterval(fetchData, 30000)
    return () => clearInterval(interval)
  }, [])

  if (loading) {
    return (
      <section className="flex h-full items-center justify-center bg-background text-foreground">
        <Card className="bg-panel">
          <CardHeader className="mb-0 gap-3">
            <div>
              <CardTitle>Cost Ledger</CardTitle>
              <CardDescription>Loading token usage and budget state</CardDescription>
            </div>
            <Badge variant="info">loading</Badge>
          </CardHeader>
        </Card>
      </section>
    )
  }

  if (error) {
    return (
      <section className="flex h-full items-center justify-center bg-background text-foreground">
        <Card className="border-status-crit bg-panel">
          <CardHeader className="mb-0 gap-3">
            <div>
              <CardTitle>Cost Ledger</CardTitle>
              <CardDescription className="text-status-crit">Error: {error}</CardDescription>
            </div>
            <StatusBadge status="error">error</StatusBadge>
          </CardHeader>
        </Card>
      </section>
    )
  }

  const s = summary!
  const b = budget!

  const formatUsd = (n: number) => `$${n.toFixed(6)}`
  const formatTokens = (n: number) => n.toLocaleString()
  const avgLatencyMs =
    s.models.length > 0
      ? s.models.reduce((a, m) => a + m.avg_latency_ms, 0) / s.models.length
      : 0
  const budgetTone = budgetStatus(s.budget_percent_used, b.alert_threshold)
  const budgetLabel =
    budgetTone === 'crit' ? 'critical' : budgetTone === 'warn' ? 'warning' : 'healthy'

  return (
    <section className="h-full overflow-y-auto bg-background p-6 text-foreground">
      <div className="mb-6 flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold tracking-tight text-foreground">Cost Ledger</h1>
            <StatusBadge status={budgetLabel}>{budgetLabel}</StatusBadge>
            <Badge variant="neutral">{s.models.length} models</Badge>
          </div>
          <p className="mt-1 text-sm text-dim">Token usage and cost tracking across the fleet</p>
        </div>
        <Button
          onClick={async () => {
            await fetch('/api/ledger/flush', { method: 'POST' })
            window.location.reload()
          }}
        >
          Flush to DB
        </Button>
      </div>

      <Card className="mb-6 bg-panel">
        <CardHeader className="items-start gap-3">
          <div>
            <CardTitle>Daily Cloud Budget</CardTitle>
            <CardDescription>
              {formatUsd(s.daily_cost_usd)} used of {formatUsd(s.daily_budget_usd)}
            </CardDescription>
          </div>
          <StatusBadge status={budgetLabel}>{s.budget_percent_used.toFixed(1)}% used</StatusBadge>
        </CardHeader>

        <div className="h-2 overflow-hidden rounded-full bg-elevated">
          <div
            className={cn('h-full rounded-full transition-all', budgetProgressClass(budgetTone))}
            style={{ width: `${Math.min(s.budget_percent_used, 100)}%` }}
          />
        </div>

        <div className="mt-4 grid gap-3 text-sm sm:grid-cols-3">
          <BudgetMetric
            label="Remaining"
            value={formatUsd(s.budget_remaining_usd)}
            tone={budgetTone === 'crit' ? 'crit' : 'ok'}
          />
          <BudgetMetric label="Cloud Cost" value={formatUsd(s.cloud_cost_usd)} tone="warn" />
          <BudgetMetric
            label="Enforcement"
            value={b.enforce_budget ? 'Enabled' : 'Monitoring'}
            tone={b.enforce_budget ? 'ok' : 'info'}
          />
        </div>
      </Card>

      <div className="mb-6 grid grid-cols-2 gap-4 md:grid-cols-4">
        <StatCard label="Total Requests" value={s.total_requests.toLocaleString()} />
        <StatCard label="Total Tokens" value={formatTokens(s.total_tokens)} />
        <StatCard label="Total Cost" value={formatUsd(s.total_cost_usd)} />
        <StatCard label="Local Requests" value={s.local_requests.toLocaleString()} tone="ok" />
        <StatCard label="Cloud Requests" value={s.cloud_requests.toLocaleString()} tone="warn" />
        <StatCard label="Cloud Cost" value={formatUsd(s.cloud_cost_usd)} tone="warn" />
        <StatCard label="Est. Savings" value={formatUsd(s.savings_vs_cloud_only_usd)} tone="ok" />
        <StatCard label="Avg Latency" value={`${avgLatencyMs.toFixed(0)} ms`} tone="info" />
      </div>

      <Card className="overflow-hidden bg-panel p-0">
        <CardHeader className="mb-0 border-b border-border px-4 py-3">
          <div>
            <CardTitle>Per-Model Breakdown</CardTitle>
            <CardDescription>Requests, tokens, cost, routing, and latency by model</CardDescription>
          </div>
          <Badge variant="neutral">{formatTokens(s.total_tokens)} tokens</Badge>
        </CardHeader>
        {s.models.length === 0 ? (
          <div className="px-4 py-8 text-center text-sm text-dim">
            No usage recorded yet. Make some LLM requests to see data here.
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-sm">
              <thead>
                <tr className="border-b border-border bg-surface text-xs text-dim">
                  <th className="px-4 py-2">Model</th>
                  <th className="px-4 py-2 text-right">Requests</th>
                  <th className="px-4 py-2 text-right">Tokens</th>
                  <th className="px-4 py-2 text-right">Cost</th>
                  <th className="px-4 py-2 text-right">Local</th>
                  <th className="px-4 py-2 text-right">Cloud</th>
                  <th className="px-4 py-2 text-right">Latency</th>
                </tr>
              </thead>
              <tbody>
                {s.models.map((m) => (
                  <tr
                    key={m.model}
                    className="border-b border-border transition last:border-0 hover:bg-elevated"
                  >
                    <td className="px-4 py-2 font-mono text-foreground">{m.model}</td>
                    <td className="px-4 py-2 text-right text-muted">
                      {m.request_count.toLocaleString()}
                    </td>
                    <td className="px-4 py-2 text-right text-muted">{formatTokens(m.total_tokens)}</td>
                    <td className="px-4 py-2 text-right text-muted">{formatUsd(m.total_cost_usd)}</td>
                    <td className="px-4 py-2 text-right text-status-ok">
                      {m.local_request_count.toLocaleString()}
                    </td>
                    <td className="px-4 py-2 text-right text-status-warn">
                      {m.cloud_request_count.toLocaleString()}
                    </td>
                    <td className="px-4 py-2 text-right text-muted">{m.avg_latency_ms.toFixed(0)} ms</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      {b && (
        <Card className="mt-6 bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Budget Configuration</CardTitle>
              <CardDescription>Current ledger guardrails and alert threshold</CardDescription>
            </div>
            <Badge variant={b.enforce_budget ? 'ok' : 'info'}>
              {b.enforce_budget ? 'enforced' : 'monitoring'}
            </Badge>
          </CardHeader>
          <div className="grid grid-cols-2 gap-4 text-sm md:grid-cols-4">
            <ConfigItem label="Daily Budget" value={formatUsd(b.daily_budget_usd)} />
            <ConfigItem label="Cloud Budget" value={formatUsd(b.cloud_daily_budget_usd)} />
            <ConfigItem label="Enforced" value={b.enforce_budget ? 'Yes' : 'No'} />
            <ConfigItem label="Alert Threshold" value={`${(b.alert_threshold * 100).toFixed(0)}%`} />
          </div>
        </Card>
      )}
    </section>
  )
}

function StatCard({
  label,
  value,
  tone = 'default',
}: {
  label: string
  value: string
  tone?: 'default' | 'ok' | 'warn' | 'crit' | 'info'
}) {
  return (
    <Card className="bg-panel p-3">
      <CardDescription>{label}</CardDescription>
      <div className={cn('mt-1 truncate text-lg font-semibold', textToneClass(tone))}>{value}</div>
    </Card>
  )
}

function BudgetMetric({
  label,
  value,
  tone,
}: {
  label: string
  value: string
  tone: 'ok' | 'warn' | 'crit' | 'info'
}) {
  return (
    <div className="rounded-lg border border-border bg-surface px-3 py-2">
      <div className="text-xs text-dim">{label}</div>
      <div className={cn('mt-1 text-sm font-semibold', textToneClass(tone))}>{value}</div>
    </div>
  )
}

function ConfigItem({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg border border-border bg-surface px-3 py-2">
      <div className="text-xs text-dim">{label}</div>
      <div className="mt-1 text-sm font-medium text-foreground">{value}</div>
    </div>
  )
}

function budgetStatus(percentUsed: number, alertThreshold: number): 'ok' | 'warn' | 'crit' {
  if (percentUsed >= 90) return 'crit'
  if (percentUsed >= alertThreshold * 100) return 'warn'
  return 'ok'
}

function budgetProgressClass(tone: 'ok' | 'warn' | 'crit') {
  if (tone === 'crit') return 'bg-status-crit'
  if (tone === 'warn') return 'bg-status-warn'
  return 'bg-status-ok'
}

function textToneClass(tone: 'default' | 'ok' | 'warn' | 'crit' | 'info') {
  if (tone === 'ok') return 'text-status-ok'
  if (tone === 'warn') return 'text-status-warn'
  if (tone === 'crit') return 'text-status-crit'
  if (tone === 'info') return 'text-status-info'
  return 'text-foreground'
}
