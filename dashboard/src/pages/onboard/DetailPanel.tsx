import { useMemo } from 'react'
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
        <ul key={`ul-${out.length}`} className="list-disc pl-5 space-y-0.5 text-slate-200">
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
          className="bg-slate-900 border border-slate-700 rounded px-3 py-2 text-[12px] font-mono text-emerald-200 overflow-x-auto"
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
      <p key={`p-${out.length}`} className="text-slate-200 leading-relaxed">
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
        <strong key={parts.length} className="text-white font-semibold">
          {tok.slice(2, -2)}
        </strong>
      )
    } else if (tok.startsWith('`')) {
      parts.push(
        <code key={parts.length} className="px-1 py-0.5 rounded bg-slate-800 text-amber-300 text-[13px] font-mono">
          {tok.slice(1, -1)}
        </code>
      )
    } else {
      parts.push(
        <a key={parts.length} href={tok} target="_blank" rel="noreferrer" className="text-indigo-300 underline">
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
      <div className="h-full p-4 text-sm text-slate-400 flex items-center justify-center">
        ← Select a checklist item to see details
      </div>
    )
  }

  return (
    <div className="h-full overflow-auto p-5">
      <div className="text-[11px] uppercase tracking-wider text-slate-500">{item.group}</div>
      <h3 className="text-lg font-semibold text-white mt-1 mb-3">{item.title}</h3>
      <div className="space-y-2">{renderMarkdown(item.detail_md)}</div>
    </div>
  )
}
