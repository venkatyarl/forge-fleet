import { useEffect, useState } from 'react'

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
      } catch (e: any) {
        setError(e.message)
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
      <div className="flex h-full items-center justify-center">
        <div className="text-zinc-400">Loading cost ledger...</div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="flex h-full items-center justify-center">
        <div className="text-red-400">Error: {error}</div>
      </div>
    )
  }

  const s = summary!
  const b = budget!

  const formatUsd = (n: number) => `$${n.toFixed(6)}`
  const formatTokens = (n: number) => n.toLocaleString()

  return (
    <div className="h-full overflow-y-auto p-6">
      <div className="mb-6 flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-zinc-100">💰 Cost Ledger</h1>
          <p className="text-sm text-zinc-400">Token usage and cost tracking across the fleet</p>
        </div>
        <div className="flex gap-2">
          <button
            onClick={async () => {
              await fetch('/api/ledger/flush', { method: 'POST' })
              window.location.reload()
            }}
            className="rounded-md bg-violet-600 px-3 py-1.5 text-sm text-white hover:bg-violet-500"
          >
            Flush to DB
          </button>
        </div>
      </div>

      {/* Budget bar */}
      <div className="mb-6 rounded-lg border border-zinc-800 bg-zinc-900/50 p-4">
        <div className="mb-2 flex items-center justify-between">
          <span className="text-sm font-medium text-zinc-300">Daily Cloud Budget</span>
          <span className="text-sm text-zinc-400">
            {formatUsd(s.daily_cost_usd)} / {formatUsd(s.daily_budget_usd)}
          </span>
        </div>
        <div className="h-2 overflow-hidden rounded-full bg-zinc-800">
          <div
            className={`h-full rounded-full transition-all ${
              s.budget_percent_used > 90 ? 'bg-red-500' : s.budget_percent_used > 70 ? 'bg-yellow-500' : 'bg-green-500'
            }`}
            style={{ width: `${Math.min(s.budget_percent_used, 100)}%` }}
          />
        </div>
        <div className="mt-1 flex justify-between text-xs text-zinc-500">
          <span>{s.budget_percent_used.toFixed(1)}% used</span>
          <span>{formatUsd(s.budget_remaining_usd)} remaining</span>
        </div>
      </div>

      {/* Summary cards */}
      <div className="mb-6 grid grid-cols-2 gap-4 md:grid-cols-4">
        <StatCard label="Total Requests" value={s.total_requests.toLocaleString()} />
        <StatCard label="Total Tokens" value={formatTokens(s.total_tokens)} />
        <StatCard label="Total Cost" value={formatUsd(s.total_cost_usd)} />
        <StatCard label="Local Requests" value={s.local_requests.toLocaleString()} color="green" />
        <StatCard label="Cloud Requests" value={s.cloud_requests.toLocaleString()} color="yellow" />
        <StatCard label="Cloud Cost" value={formatUsd(s.cloud_cost_usd)} color="yellow" />
        <StatCard label="Est. Savings" value={formatUsd(s.savings_vs_cloud_only_usd)} color="green" />
        <StatCard label="Avg Latency" value={`${s.models.length > 0 ? (s.models.reduce((a, m) => a + m.avg_latency_ms, 0) / s.models.length).toFixed(0) : 0} ms`} />
      </div>

      {/* Model breakdown */}
      <div className="rounded-lg border border-zinc-800 bg-zinc-900/50">
        <div className="border-b border-zinc-800 px-4 py-3">
          <h2 className="text-sm font-semibold text-zinc-200">Per-Model Breakdown</h2>
        </div>
        {s.models.length === 0 ? (
          <div className="px-4 py-8 text-center text-sm text-zinc-500">
            No usage recorded yet. Make some LLM requests to see data here.
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-sm">
              <thead>
                <tr className="border-b border-zinc-800 text-xs text-zinc-500">
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
                  <tr key={m.model} className="border-b border-zinc-800/50 hover:bg-zinc-800/30">
                    <td className="px-4 py-2 font-mono text-zinc-300">{m.model}</td>
                    <td className="px-4 py-2 text-right text-zinc-400">{m.request_count.toLocaleString()}</td>
                    <td className="px-4 py-2 text-right text-zinc-400">{formatTokens(m.total_tokens)}</td>
                    <td className="px-4 py-2 text-right text-zinc-400">{formatUsd(m.total_cost_usd)}</td>
                    <td className="px-4 py-2 text-right text-green-400">{m.local_request_count.toLocaleString()}</td>
                    <td className="px-4 py-2 text-right text-yellow-400">{m.cloud_request_count.toLocaleString()}</td>
                    <td className="px-4 py-2 text-right text-zinc-400">{m.avg_latency_ms.toFixed(0)} ms</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* Budget config */}
      {b && (
        <div className="mt-6 rounded-lg border border-zinc-800 bg-zinc-900/50 p-4">
          <h2 className="mb-2 text-sm font-semibold text-zinc-200">Budget Configuration</h2>
          <div className="grid grid-cols-2 gap-4 text-sm md:grid-cols-4">
            <div>
              <span className="text-zinc-500">Daily Budget</span>
              <div className="text-zinc-300">{formatUsd(b.daily_budget_usd)}</div>
            </div>
            <div>
              <span className="text-zinc-500">Cloud Budget</span>
              <div className="text-zinc-300">{formatUsd(b.cloud_daily_budget_usd)}</div>
            </div>
            <div>
              <span className="text-zinc-500">Enforced</span>
              <div className="text-zinc-300">{b.enforce_budget ? 'Yes' : 'No'}</div>
            </div>
            <div>
              <span className="text-zinc-500">Alert Threshold</span>
              <div className="text-zinc-300">{(b.alert_threshold * 100).toFixed(0)}%</div>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}

function StatCard({
  label,
  value,
  color = 'default',
}: {
  label: string
  value: string
  color?: 'default' | 'green' | 'yellow' | 'red'
}) {
  const colorClass =
    color === 'green'
      ? 'text-green-400'
      : color === 'yellow'
      ? 'text-yellow-400'
      : color === 'red'
      ? 'text-red-400'
      : 'text-zinc-200'

  return (
    <div className="rounded-lg border border-zinc-800 bg-zinc-900/50 p-3">
      <div className="text-xs text-zinc-500">{label}</div>
      <div className={`mt-1 text-lg font-semibold ${colorClass}`}>{value}</div>
    </div>
  )
}
