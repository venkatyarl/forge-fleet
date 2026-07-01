import { QueryClient } from '@tanstack/react-query'
import type { DashboardEvent, FleetNode, LlmServer, WorkItem, AgentRun, AgentRunEvent, Alert } from '../protocol/types'

export const QUERY_KEYS = {
  snapshot: ['dashboard', 'snapshot'] as const,
  fleet: ['dashboard', 'fleet'] as const,
  llmServers: ['dashboard', 'llm-servers'] as const,
  workItems: ['dashboard', 'work-items'] as const,
  agents: ['dashboard', 'agents'] as const,
  sessions: ['dashboard', 'sessions'] as const,
  alerts: ['dashboard', 'alerts'] as const,
  skills: ['dashboard', 'skills'] as const,
}

export function reduceDashboardEvent(queryClient: QueryClient, event: DashboardEvent): void {
  switch (event.kind) {
    case 'FleetUpdated':
      queryClient.setQueryData<FleetNode[]>(QUERY_KEYS.fleet, (prev = []) =>
        upsertById(prev, event.node, (n) => n.id)
      )
      return

    case 'LlmServerUpdated':
      queryClient.setQueryData<LlmServer[]>(QUERY_KEYS.llmServers, (prev = []) =>
        upsertById(prev, event.server, (s) => s.id)
      )
      return

    case 'WorkItemChanged':
      queryClient.setQueryData<WorkItem[]>(QUERY_KEYS.workItems, (prev = []) =>
        upsertById(prev, event.item, (w) => w.id)
      )
      return

    case 'AgentEvent': {
      queryClient.setQueryData<AgentRun[]>(QUERY_KEYS.agents, (prev = []) => {
        const next = [...prev]
        const idx = next.findIndex((r) => r.id === event.runId)
        if (idx >= 0) {
          next[idx] = applyAgentEvent(next[idx], event.event)
        } else {
          next.push({
            id: event.runId,
            sessionId: event.runId,
            status: 'running',
            model: 'auto',
            backend: 'local',
            turn: 0,
            lastActivity: '',
          })
        }
        return next
      })
      return
    }

    case 'AlertFired':
      queryClient.setQueryData<Alert[]>(QUERY_KEYS.alerts, (prev = []) =>
        upsertById(prev, event.alert, (a) => a.id)
      )
      return

    case 'AlertCleared':
      queryClient.setQueryData<Alert[]>(QUERY_KEYS.alerts, (prev = []) =>
        prev.map((a) => (a.id === event.alertId ? { ...a, clearedAt: new Date().toISOString() } : a))
      )
      return

    case 'ResyncRequired':
      queryClient.invalidateQueries({ queryKey: QUERY_KEYS.snapshot })
      return

    case 'Heartbeat':
    default:
      return
  }
}

function upsertById<T>(list: T[], item: T, getId: (item: T) => string): T[] {
  const id = getId(item)
  const idx = list.findIndex((existing) => getId(existing) === id)
  if (idx >= 0) {
    const next = [...list]
    next[idx] = item
    return next
  }
  return [...list, item]
}

function applyAgentEvent(run: AgentRun, event: AgentRunEvent): AgentRun {
  switch (event.kind) {
    case 'Done':
      return { ...run, status: 'done' }
    case 'Error':
      return { ...run, status: 'error', lastActivity: event.message }
    case 'Status':
      return { ...run, lastActivity: event.message }
    case 'ToolStart':
      return { ...run, lastActivity: `Running tool: ${event.toolName}` }
    case 'ToolEnd': {
      const mark = event.isError ? '✗' : '✓'
      return { ...run, lastActivity: `${mark} ${event.toolName} (${event.durationMs} ms)` }
    }
    case 'TurnComplete':
      return { ...run, turn: event.turn }
    case 'AssistantText':
      return { ...run, lastActivity: 'Assistant replied' }
    case 'Compaction':
      return {
        ...run,
        lastActivity: `Compacted history ${event.messagesBefore}→${event.messagesAfter}`,
      }
    case 'TokenWarning':
      return { ...run, lastActivity: `Context: ${event.usagePct.toFixed(0)}%` }
    default:
      return run
  }
}
