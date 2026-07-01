import { useCallback, useEffect, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { cn } from '../lib/utils'

type McpTool = {
  name: string
  description?: string
  inputSchema?: Record<string, unknown>
  'x-federated'?: boolean
  'x-source-service'?: string
  'x-source-endpoint'?: string
}

export function Mcp() {
  const [tools, setTools] = useState<McpTool[]>([])
  const [health, setHealth] = useState<'ok' | 'error' | 'checking'>('checking')
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [selectedTool, setSelectedTool] = useState<McpTool | null>(null)

  const load = useCallback(async () => {
    setLoading(true)
    setError(null)
    setHealth('checking')
    try {
      const healthRes = await fetch('/mcp/health')
      setHealth(healthRes.ok ? 'ok' : 'error')
    } catch {
      setHealth('error')
    }

    try {
      const res = await fetch('/mcp', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/list', params: {} }),
      })
      const payload = await res.json()
      setTools(Array.isArray(payload.result?.tools) ? (payload.result.tools as McpTool[]) : [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load MCP tools')
      setTools([])
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  const federatedCount = tools.filter((t) => t['x-federated']).length
  const localCount = tools.length - federatedCount

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">MCP</h1>
            {health === 'ok' ? <Badge variant="ok">healthy</Badge> : <Badge variant="crit">unreachable</Badge>}
          </div>
          <p className="mt-1 text-sm text-muted">
            Model Context Protocol server discovery and tool inventory.
          </p>
        </div>
        <Button onClick={() => void load()} disabled={loading} type="button" variant="outline">
          Refresh
        </Button>
      </div>

      <div className="grid gap-3 sm:grid-cols-3">
        <SummaryCard label="Tools" value={tools.length.toLocaleString()} detail="tools/list result" />
        <SummaryCard label="Local" value={localCount.toLocaleString()} detail="native tools" />
        <SummaryCard label="Federated" value={federatedCount.toLocaleString()} detail="remote services" />
      </div>

      {error ? (
        <Card className="border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">{error}</Card>
      ) : null}

      <div className="grid gap-4 xl:grid-cols-12">
        <div className="xl:col-span-5">
          <Card className="bg-panel">
            <CardHeader>
              <div>
                <CardTitle>Tool Registry</CardTitle>
                <CardDescription>{tools.length} tool{tools.length === 1 ? '' : 's'} available</CardDescription>
              </div>
            </CardHeader>
            {loading && tools.length === 0 ? (
              <p className="text-sm text-dim">Loading MCP tools...</p>
            ) : tools.length === 0 ? (
              <p className="text-sm text-dim">No MCP tools returned.</p>
            ) : (
              <div className="max-h-[640px] space-y-1 overflow-y-auto pr-1">
                {tools.map((tool) => (
                  <button
                    key={tool.name}
                    type="button"
                    onClick={() => setSelectedTool(tool)}
                    className={cn(
                      'w-full rounded-lg border p-3 text-left transition',
                      selectedTool?.name === tool.name
                        ? 'border-primary bg-primary-subtle'
                        : 'border-border bg-surface hover:border-border-subtle hover:bg-elevated',
                    )}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="font-mono text-sm text-status-info">{tool.name}</span>
                      {tool['x-federated'] ? <Badge variant="info">federated</Badge> : <Badge variant="neutral">local</Badge>}
                    </div>
                    {tool.description ? (
                      <p className="mt-1 line-clamp-2 text-xs text-muted">{tool.description}</p>
                    ) : null}
                  </button>
                ))}
              </div>
            )}
          </Card>
        </div>

        <div className="xl:col-span-7">
          <Card className="bg-panel">
            <CardHeader>
              <div>
                <CardTitle>Tool Detail</CardTitle>
                <CardDescription>Select a tool to inspect its input schema.</CardDescription>
              </div>
            </CardHeader>
            {!selectedTool ? (
              <p className="text-sm text-dim">Select a tool to view details.</p>
            ) : (
              <div className="space-y-3">
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant="default">{selectedTool.name}</Badge>
                  {selectedTool['x-source-service'] ? (
                    <Badge variant="neutral">{selectedTool['x-source-service']}</Badge>
                  ) : null}
                  {selectedTool['x-source-endpoint'] ? (
                    <Badge variant="neutral">{selectedTool['x-source-endpoint']}</Badge>
                  ) : null}
                </div>
                {selectedTool.description ? <p className="text-sm text-muted">{selectedTool.description}</p> : null}
                {selectedTool.inputSchema ? (
                  <pre className="max-h-[520px] overflow-auto rounded-lg border border-border bg-background p-3 font-mono text-xs text-foreground">
                    {JSON.stringify(selectedTool.inputSchema, null, 2)}
                  </pre>
                ) : (
                  <p className="text-sm text-dim">No input schema provided.</p>
                )}
              </div>
            )}
          </Card>
        </div>
      </div>
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
