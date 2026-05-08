import { useEffect, useState } from 'react'
import { getJson } from '../lib/api'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type UnifiedModel = {
  name: string
  params?: string
  context?: string
  bestFor?: string
  sources: ModelSource[]
  /** Fleet-specific */
  fleetMember?: string
  fleetIp?: string
  fleetPort?: number
  online?: boolean
}

type ModelSource = {
  name: 'Fleet' | 'HuggingFace' | 'Ollama'
  url?: string
  downloads?: number
  likes?: number
}

type LlmServer = {
  computer: string
  endpoint: string
  runtime: string
  model: string
  queue_depth: number
  tokens_per_sec: number
  healthy: boolean
  status: string
}

// ---------------------------------------------------------------------------
// Static reference models (well-known external models)
// ---------------------------------------------------------------------------

const REFERENCE_MODELS: UnifiedModel[] = [
  { name: 'Llama-3.1-405B', params: '405B', context: '128K', bestFor: 'Maximum quality', sources: [
    { name: 'HuggingFace', url: 'https://huggingface.co/meta-llama/Llama-3.1-405B' },
    { name: 'Ollama', url: 'https://ollama.com/library/llama3.1:405b' },
  ]},
  { name: 'Llama-3.1-70B', params: '70B', context: '128K', bestFor: 'High quality, balanced', sources: [
    { name: 'HuggingFace', url: 'https://huggingface.co/meta-llama/Llama-3.1-70B' },
    { name: 'Ollama', url: 'https://ollama.com/library/llama3.1:70b' },
  ]},
  { name: 'DeepSeek-Coder-V2', params: '236B', context: '128K', bestFor: 'Coding', sources: [
    { name: 'HuggingFace', url: 'https://huggingface.co/deepseek-ai/DeepSeek-Coder-V2-Instruct' },
  ]},
  { name: 'Mistral-Large-2', params: '123B', context: '128K', bestFor: 'Reasoning, Multilingual', sources: [
    { name: 'HuggingFace', url: 'https://huggingface.co/mistralai/Mistral-Large-Instruct-2407' },
    { name: 'Ollama', url: 'https://ollama.com/library/mistral-large' },
  ]},
  { name: 'Phi-4', params: '14B', context: '16K', bestFor: 'Efficient, edge deployment', sources: [
    { name: 'HuggingFace', url: 'https://huggingface.co/microsoft/phi-4' },
    { name: 'Ollama', url: 'https://ollama.com/library/phi4' },
  ]},
  { name: 'Gemma-2-27B', params: '27B', context: '8K', bestFor: 'Efficient reasoning', sources: [
    { name: 'HuggingFace', url: 'https://huggingface.co/google/gemma-2-27b-it' },
    { name: 'Ollama', url: 'https://ollama.com/library/gemma2:27b' },
  ]},
  { name: 'CodeLlama-34B', params: '34B', context: '16K', bestFor: 'Code generation', sources: [
    { name: 'HuggingFace', url: 'https://huggingface.co/codellama/CodeLlama-34b-Instruct-hf' },
    { name: 'Ollama', url: 'https://ollama.com/library/codellama:34b' },
  ]},
  { name: 'Qwen2.5-Coder-32B', params: '32B', context: '32K', bestFor: 'Coding, tool calling', sources: [
    { name: 'HuggingFace', url: 'https://huggingface.co/Qwen/Qwen2.5-Coder-32B-Instruct' },
    { name: 'Ollama', url: 'https://ollama.com/library/qwen2.5-coder:32b' },
  ]},
]

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function parseParamsFromModelName(name: string): string | undefined {
  // Try to extract parameter count from common naming patterns
  const m = name.match(/-(\d+\.?\d*)B/i)
  return m ? m[1] + 'B' : undefined
}

function parseContextFromName(_name: string): string | undefined {
  // Most llama.cpp/Qwen models default to 32K; Gemma-4 is 262K
  return undefined // Let the table show '—' unless we have real data
}

function endpointToIpPort(endpoint: string): { ip: string; port: number } | null {
  try {
    const url = new URL(endpoint)
    return { ip: url.hostname, port: parseInt(url.port, 10) || 80 }
  } catch {
    return null
  }
}

// ---------------------------------------------------------------------------
// Main component
// ---------------------------------------------------------------------------

export function ModelHub() {
  const [query, setQuery] = useState('')
  const [models, setModels] = useState<UnifiedModel[]>([])
  const [showFleetOnly, setShowFleetOnly] = useState(false)
  const [loading, setLoading] = useState(true)

  // Fetch live fleet models + merge with reference
  useEffect(() => {
    let alive = true
    setLoading(true)

    getJson<{ servers: LlmServer[] }>('/api/llm/servers')
      .then((res) => {
        if (!alive) return
        const fleetModels: UnifiedModel[] = (res.servers || []).map((s) => {
          const ep = endpointToIpPort(s.endpoint)
          return {
            name: s.model,
            params: parseParamsFromModelName(s.model),
            context: parseContextFromName(s.model),
            bestFor: s.runtime,
            fleetMember: s.computer,
            fleetIp: ep?.ip,
            fleetPort: ep?.port,
            online: s.healthy,
            sources: [{ name: 'Fleet' }],
          }
        })
        setModels([...fleetModels, ...REFERENCE_MODELS])
      })
      .catch(() => {
        if (!alive) return
        // Fallback to reference models only
        setModels([...REFERENCE_MODELS])
      })
      .finally(() => {
        if (alive) setLoading(false)
      })

    return () => { alive = false }
  }, [])

  // Filter
  const filtered = models.filter((m) => {
    if (showFleetOnly && !m.fleetMember) return false
    if (!query) return true
    const lower = query.toLowerCase()
    return (
      m.name.toLowerCase().includes(lower) ||
      (m.bestFor ?? '').toLowerCase().includes(lower) ||
      (m.fleetMember ?? '').toLowerCase().includes(lower) ||
      (m.params ?? '').toLowerCase().includes(lower) ||
      m.sources.some((s) => s.name.toLowerCase().includes(lower))
    )
  })

  return (
    <section className="space-y-4">
      <div>
        <h1 className="text-2xl font-bold text-zinc-100">Available Models</h1>
        <p className="text-sm text-zinc-500">
          Models running on your fleet and available from external sources
        </p>
      </div>

      {/* Search + filter */}
      <div className="flex items-center gap-3">
        <div className="relative flex-1">
          <svg
            className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-zinc-500"
            fill="none"
            viewBox="0 0 24 24"
            stroke="currentColor"
          >
            <path
              strokeLinecap="round"
              strokeLinejoin="round"
              strokeWidth={2}
              d="M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z"
            />
          </svg>
          <input
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search models by name, params, use case..."
            className="w-full rounded-lg border border-zinc-700 bg-zinc-950 pl-10 pr-3 py-2 text-sm text-zinc-100 placeholder:text-zinc-500 focus:border-violet-500/50 focus:outline-none"
          />
        </div>
        <button
          onClick={() => setShowFleetOnly(!showFleetOnly)}
          className={`rounded-lg border px-3 py-2 text-sm transition ${
            showFleetOnly
              ? 'border-violet-500/50 bg-violet-500/20 text-violet-300'
              : 'border-zinc-700 bg-zinc-900 text-zinc-400 hover:text-zinc-200'
          }`}
        >
          Fleet only
        </button>
      </div>

      {/* Results count */}
      <p className="text-xs text-zinc-500">
        {loading ? 'Loading...' : `${filtered.length} model${filtered.length !== 1 ? 's' : ''} found`}
      </p>

      {/* Table */}
      <div className="overflow-x-auto rounded-xl border border-zinc-800 bg-zinc-900/60">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-zinc-800 text-left">
              <th className="px-4 py-3 text-xs font-semibold uppercase tracking-wider text-zinc-500">
                Model
              </th>
              <th className="px-4 py-3 text-xs font-semibold uppercase tracking-wider text-zinc-500">
                Params
              </th>
              <th className="px-4 py-3 text-xs font-semibold uppercase tracking-wider text-zinc-500">
                Context
              </th>
              <th className="px-4 py-3 text-xs font-semibold uppercase tracking-wider text-zinc-500">
                Best For
              </th>
              <th className="px-4 py-3 text-xs font-semibold uppercase tracking-wider text-zinc-500">
                Fleet Member
              </th>
              <th className="px-4 py-3 text-xs font-semibold uppercase tracking-wider text-zinc-500">
                Sources
              </th>
              <th className="px-4 py-3 text-xs font-semibold uppercase tracking-wider text-zinc-500">
                Status
              </th>
            </tr>
          </thead>
          <tbody>
            {filtered.map((m, i) => (
              <tr
                key={`${m.name}-${m.fleetMember ?? i}`}
                className="border-b border-zinc-800/50 transition hover:bg-zinc-800/30"
              >
                <td className="px-4 py-3">
                  <span className="font-medium text-zinc-200">{m.name}</span>
                </td>
                <td className="px-4 py-3 text-zinc-400">{m.params ?? '—'}</td>
                <td className="px-4 py-3 text-zinc-400">{m.context ?? '—'}</td>
                <td className="px-4 py-3 text-zinc-400">{m.bestFor ?? '—'}</td>
                <td className="px-4 py-3">
                  {m.fleetMember ? (
                    <span className="text-zinc-200">{m.fleetMember}</span>
                  ) : (
                    <span className="text-zinc-600">—</span>
                  )}
                </td>
                <td className="px-4 py-3">
                  <div className="flex flex-wrap gap-1">
                    {m.sources.map((s) => (
                      <SourceBadge key={s.name} source={s} />
                    ))}
                  </div>
                </td>
                <td className="px-4 py-3">
                  {m.fleetMember ? (
                    <span
                      className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs ${
                        m.online
                          ? 'bg-emerald-500/20 text-emerald-300'
                          : 'bg-zinc-700/50 text-zinc-400'
                      }`}
                    >
                      <span
                        className={`h-1.5 w-1.5 rounded-full ${
                          m.online ? 'bg-emerald-400' : 'bg-zinc-500'
                        }`}
                      />
                      {m.online ? 'Online' : 'Offline'}
                    </span>
                  ) : (
                    <span className="text-xs text-zinc-600">Available</span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>

        {filtered.length === 0 && !loading && (
          <div className="px-4 py-8 text-center text-sm text-zinc-500">
            No models match &ldquo;{query}&rdquo;
          </div>
        )}
      </div>
    </section>
  )
}

// ---------------------------------------------------------------------------
// Source badge with link
// ---------------------------------------------------------------------------

function SourceBadge({ source }: { source: ModelSource }) {
  const colors: Record<string, string> = {
    Fleet: 'bg-violet-500/20 text-violet-300 border-violet-500/30',
    HuggingFace: 'bg-amber-500/20 text-amber-300 border-amber-500/30',
    Ollama: 'bg-sky-500/20 text-sky-300 border-sky-500/30',
  }

  const badge = (
    <span
      className={`inline-flex items-center rounded-md border px-1.5 py-0.5 text-[10px] font-medium ${
        colors[source.name] ?? 'bg-zinc-700 text-zinc-300 border-zinc-600'
      }`}
    >
      {source.name}
    </span>
  )

  if (source.url) {
    return (
      <a
        href={source.url}
        target="_blank"
        rel="noopener noreferrer"
        className="hover:opacity-80 transition"
      >
        {badge}
      </a>
    )
  }

  return badge
}
