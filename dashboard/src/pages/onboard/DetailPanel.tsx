import { useMemo } from 'react'
import { Badge } from '../../components/ui/badge'
import { Card, CardDescription, CardHeader, CardTitle } from '../../components/ui/card'
import type { ChecklistItem } from '../../data/onboardChecklist'
import { CHECKLIST } from '../../data/onboardChecklist'

interface DetailPanelProps {
  activeId: string | null
}

/**
 * Extremely minimal markdown renderer — sufficient for the checklist
 * detail strings, which use only bold, inline code, fenced blocks, and
 * bullet lists. Avoids adding react-markdown as a dep.
 */
function renderMarkdown(md: string): React.ReactNode {
  const lines = md.split('\n')
  const out: React.ReactNode[] = []
  let inFence = false
  let fenceBuf: string[] = []
  let listBuf: string[] = []

  const flushList = () => {
    if (listBuf.length) {
      out.push(
        <ul key={`ul-${out.length}`} className="list-disc space-y-1 pl-5 text-foreground">
          {listBuf.map((l, i) => (
            <li key={i}>{renderInline(l)}</li>
          ))}
        </ul>
      )
      listBuf = []
    }
  }
  const flushFence = () => {
    if (fenceBuf.length) {
      out.push(
        <pre
          key={`pre-${out.length}`}
          className="overflow-x-auto rounded-lg border border-border bg-background px-3 py-2 font-mono text-xs leading-5 text-status-ok"
        >
          {fenceBuf.join('\n')}
        </pre>
      )
      fenceBuf = []
    }
  }

  for (const raw of lines) {
    if (raw.startsWith('```')) {
      if (inFence) {
        flushFence()
        inFence = false
      } else {
        flushList()
        inFence = true
      }
      continue
    }
    if (inFence) {
      fenceBuf.push(raw)
      continue
    }
    const line = raw
    if (/^\s*[-*]\s+/.test(line)) {
      listBuf.push(line.replace(/^\s*[-*]\s+/, ''))
      continue
    }
    flushList()
    if (line.trim() === '') {
      out.push(<div key={`br-${out.length}`} className="h-2" />)
      continue
    }
    out.push(
      <p key={`p-${out.length}`} className="leading-relaxed text-foreground">
        {renderInline(line)}
      </p>
    )
  }
  flushList()
  flushFence()
  return out
}

function renderInline(s: string): React.ReactNode {
  const parts: React.ReactNode[] = []
  let idx = 0
  const regex = /(\*\*[^*]+\*\*)|(`[^`]+`)|(https?:\/\/\S+)/g
  let m: RegExpExecArray | null
  while ((m = regex.exec(s)) !== null) {
    if (m.index > idx) parts.push(s.slice(idx, m.index))
    const tok = m[0]
    if (tok.startsWith('**')) {
      parts.push(
        <strong key={parts.length} className="font-semibold text-foreground">
          {tok.slice(2, -2)}
        </strong>
      )
    } else if (tok.startsWith('`')) {
      parts.push(
        <code
          key={parts.length}
          className="rounded bg-elevated px-1 py-0.5 font-mono text-[13px] text-primary"
        >
          {tok.slice(1, -1)}
        </code>
      )
    } else {
      parts.push(
        <a key={parts.length} href={tok} target="_blank" rel="noreferrer" className="text-primary underline">
          {tok}
        </a>
      )
    }
    idx = m.index + tok.length
  }
  if (idx < s.length) parts.push(s.slice(idx))
  return parts
}

export function DetailPanel({ activeId }: DetailPanelProps) {
  const item: ChecklistItem | undefined = useMemo(
    () => CHECKLIST.find((i) => i.id === activeId),
    [activeId]
  )

  if (!item) {
    return (
      <div className="flex h-full items-center justify-center p-4">
        <Card className="max-w-sm bg-panel text-center">
          <CardTitle>Select a checklist item</CardTitle>
          <CardDescription className="mt-2">Step details will appear here.</CardDescription>
        </Card>
      </div>
    )
  }

  return (
    <div className="h-full overflow-auto p-5">
      <Card className="bg-panel">
        <CardHeader className="items-start gap-3">
          <div>
            <CardTitle className="text-base">{item.title}</CardTitle>
            <CardDescription>Checklist detail and commands.</CardDescription>
          </div>
          <Badge variant="neutral">{item.group}</Badge>
        </CardHeader>
        <div className="space-y-3 text-sm">{renderMarkdown(item.detail_md)}</div>
      </Card>
    </div>
  )
}
