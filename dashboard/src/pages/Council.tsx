import { useCallback, useEffect, useMemo, useState } from 'react'
import { Badge } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { StatusBadge } from '../components/ui/status-badge'

type Interaction = {
  channel: string
  engine?: string
  request_text: string
  response_text?: string
  error_text?: string
  outcome?: string
  latency_ms?: number
  tokens_in?: number
  tokens_out?: number
  ts?: string
  created_at?: string
}

type ListPayload = {
  rows: Interaction[]
  error?: string
}

const DEFAULT_MEMBERS = ['codex', 'kimi']

function fmtTs(raw?: string) {
  if (!raw) return '-'
  try {
    return new Date(raw).toLocaleString()
  } catch {
    return raw
  }
}

function outcomeTone(outcome?: string) {
  const o = (outcome ?? '').toLowerCase()
  if (o === 'success' || o === 'ok') return 'ok'
  if (o === 'error' || o === 'failure' || o === 'failed') return 'crit'
  return 'neutral'
}

export function Council() {
  const [memberRows, setMemberRows] = useState<Interaction[]>([])
  const [chairRows, setChairRows] = useState<Interaction[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [topic, setTopic] = useState('')
  const [members, setMembers] = useState(DEFAULT_MEMBERS.join(', '))
  const [copied, setCopied] = useState(false)

  const load = useCallback(async () => {
    try {
      setError(null)
      const [memberRes, chairRes] = await Promise.all([
        fetch('/api/interactions?limit=50&channel=council_member').then((r) => r.json() as Promise<ListPayload>),
        fetch('/api/interactions?limit=20&channel=council_chairman').then((r) => r.json() as Promise<ListPayload>),
      ])
      setMemberRows(memberRes.rows || [])
      setChairRows(chairRes.rows || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load council interactions')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
    const id = window.setInterval(() => void load(), 5000)
    return () => window.clearInterval(id)
  }, [load])

  const cliCommand = useMemo(() => {
    const membersArg = members.trim() || DEFAULT_MEMBERS.join(',')
    const cleanTopic = topic.trim().replace(/"/g, '\\"')
    return `ff council --members ${membersArg} -- "${cleanTopic}"`
  }, [members, topic])

  const copyCommand = async () => {
    try {
      await navigator.clipboard.writeText(cliCommand)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1500)
    } catch {
      // ignore
    }
  }

  const latestConsensus = chairRows[0]?.response_text

  return (
    <section className="space-y-5 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold text-foreground">LLM Council</h1>
            {loading ? <Badge variant="info">loading</Badge> : <Badge variant="ok">live</Badge>}
          </div>
          <p className="mt-1 text-sm text-muted">
            Multi-model deliberation surface. Recent councils are read from <code className="rounded-sm bg-elevated px-1 font-mono text-primary">ff_interactions</code>.
          </p>
        </div>
        <Button onClick={() => void load()} disabled={loading} type="button" variant="outline">
          Refresh
        </Button>
      </div>

      <div className="grid gap-3 sm:grid-cols-3">
        <SummaryCard label="Member Answers" value={memberRows.length.toLocaleString()} detail="council_member channel" />
        <SummaryCard label="Chairman Calls" value={chairRows.length.toLocaleString()} detail="council_chairman channel" />
        <SummaryCard label="Latest Consensus" value={latestConsensus ? 'available' : 'none'} detail="most recent synthesis" />
      </div>

      {error ? (
        <Card className="border-status-crit bg-panel px-4 py-3 text-sm text-status-crit">{error}</Card>
      ) : null}

      <Card className="bg-panel">
        <CardHeader>
          <div>
            <CardTitle>Convene Council</CardTitle>
            <CardDescription>
              Compose a topic and copy the CLI command. Councils run through the terminal today.
            </CardDescription>
          </div>
        </CardHeader>
        <div className="space-y-3">
          <textarea
            aria-label="Council topic"
            value={topic}
            onChange={(e) => setTopic(e.target.value)}
            placeholder="Topic or question for the council..."
            rows={3}
            className={fieldClass}
          />
          <input
            aria-label="Council members"
            value={members}
            onChange={(e) => setMembers(e.target.value)}
            placeholder="Members (comma-separated)"
            className={fieldClass}
          />
          <div className="flex items-center gap-2">
            <code className="min-w-0 flex-1 truncate rounded-lg border border-border bg-background px-3 py-2 font-mono text-xs text-foreground">
              {cliCommand}
            </code>
            <Button type="button" onClick={copyCommand} disabled={!topic.trim()} size="sm">
              {copied ? 'Copied' : 'Copy'}
            </Button>
          </div>
        </div>
      </Card>

      {latestConsensus && (
        <Card className="border-status-ok bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Latest Consensus</CardTitle>
              <CardDescription>{fmtTs(chairRows[0]?.ts ?? chairRows[0]?.created_at)}</CardDescription>
            </div>
            <StatusBadge status="ok">synthesized</StatusBadge>
          </CardHeader>
          <p className="whitespace-pre-wrap text-sm text-foreground">{latestConsensus}</p>
        </Card>
      )}

      <div className="grid gap-4 xl:grid-cols-2">
        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Member Answers</CardTitle>
              <CardDescription>{memberRows.length} recorded answer{memberRows.length === 1 ? '' : 's'}</CardDescription>
            </div>
          </CardHeader>
          {loading && memberRows.length === 0 ? (
            <p className="text-sm text-dim">Loading member answers...</p>
          ) : memberRows.length === 0 ? (
            <p className="text-sm text-dim">No council member answers captured yet.</p>
          ) : (
            <div className="max-h-[520px] space-y-2 overflow-y-auto pr-1">
              {memberRows.map((row, idx) => (
                <div key={idx} className="rounded-lg border border-border bg-surface p-3">
                  <div className="flex flex-wrap items-center gap-2">
                    {row.engine ? <Badge variant="default">{row.engine}</Badge> : null}
                    {row.latency_ms != null ? <Badge variant="neutral">{row.latency_ms} ms</Badge> : null}
                    {row.outcome ? <StatusBadge status={outcomeTone(row.outcome)}>{row.outcome}</StatusBadge> : null}
                    <span className="ml-auto text-xs text-dim">{fmtTs(row.ts ?? row.created_at)}</span>
                  </div>
                  <p className="mt-2 whitespace-pre-wrap text-sm text-muted">
                    {row.response_text || row.error_text || <em className="text-dim">(empty)</em>}
                  </p>
                </div>
              ))}
            </div>
          )}
        </Card>

        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Chairman Syntheses</CardTitle>
              <CardDescription>{chairRows.length} recorded synthesis{chairRows.length === 1 ? '' : 's'}</CardDescription>
            </div>
          </CardHeader>
          {loading && chairRows.length === 0 ? (
            <p className="text-sm text-dim">Loading chairman calls...</p>
          ) : chairRows.length === 0 ? (
            <p className="text-sm text-dim">No chairman syntheses captured yet.</p>
          ) : (
            <div className="max-h-[520px] space-y-2 overflow-y-auto pr-1">
              {chairRows.map((row, idx) => (
                <div key={idx} className="rounded-lg border border-border bg-surface p-3">
                  <div className="flex flex-wrap items-center gap-2">
                    {row.engine ? <Badge variant="default">{row.engine}</Badge> : null}
                    {row.outcome ? <StatusBadge status={outcomeTone(row.outcome)}>{row.outcome}</StatusBadge> : null}
                    <span className="ml-auto text-xs text-dim">{fmtTs(row.ts ?? row.created_at)}</span>
                  </div>
                  <p className="mt-2 whitespace-pre-wrap text-sm text-foreground">
                    {row.response_text || <em className="text-dim">(empty)</em>}
                  </p>
                </div>
              ))}
            </div>
          )}
        </Card>
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

const fieldClass =
  'min-h-9 w-full rounded-lg border border-border bg-elevated px-3 py-2 text-sm text-foreground outline-hidden transition placeholder:text-dim focus:border-primary disabled:cursor-not-allowed disabled:opacity-60'
