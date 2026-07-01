import { useQuery } from '@tanstack/react-query'
import { getJson } from '../../lib/api'
import { extractNodes, extractSummary } from '../../lib/normalizers'
import type { FleetStatusResponse, FleetComputer, FleetStatusSummary } from '../../types'
import type { WorkItem } from '../../protocol/types'
import { QUERY_KEYS } from '../../sync/events'

export function useFleetStatus() {
  return useQuery({
    queryKey: QUERY_KEYS.fleet,
    queryFn: async () => {
      const data = await getJson<FleetStatusResponse>('/api/fleet/status').catch(() =>
        getJson<FleetStatusResponse>('/api/status')
      )
      return {
        nodes: extractNodes(data),
        summary: extractSummary(data),
      }
    },
  })
}

export function useWorkItems(limit?: number) {
  return useQuery<WorkItem[]>({
    queryKey: QUERY_KEYS.workItems,
    queryFn: async () => {
      const items = await getJson<WorkItem[]>('/api/mc/work-items').catch(() => [])
      return Array.isArray(items) ? (limit ? items.slice(0, limit) : items) : []
    },
  })
}

export function useAgentSessions<T = ChatSummary[]>(select?: (data: ChatSummary[]) => T) {
  return useQuery({
    queryKey: QUERY_KEYS.sessions,
    queryFn: async () => {
      const data = await getJson<ChatSummary[]>('/api/agent/sessions').catch(() => [])
      return Array.isArray(data) ? data : []
    },
    select,
  })
}

export function useAuditRecent(limit = 10) {
  return useQuery({
    queryKey: [...QUERY_KEYS.alerts, 'audit', limit],
    queryFn: async () => {
      const data = await getJson<AuditEntry[]>(`/api/audit/recent?limit=${limit}`).catch(() => [])
      return Array.isArray(data) ? data : []
    },
  })
}

type ChatSummary = {
  id: string
  title?: string
  model?: string
  created_at?: string
  message_count?: number
}

type AuditEntry = {
  id?: string
  event_type: string
  actor?: string
  details_json?: string
  timestamp?: string
  created_at?: string
}

export type { FleetComputer, FleetStatusSummary }
