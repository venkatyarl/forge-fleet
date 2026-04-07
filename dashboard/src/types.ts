export type HealthState = 'healthy' | 'degraded' | 'offline' | 'unknown'
export type NodeStatus = 'online' | 'degraded' | 'offline' | 'unknown'
export type LeaderState = 'leader' | 'follower' | 'unknown'

export type FleetModel = {
  id?: string
  name: string
  tier?: number
  status?: string
  contextWindow?: number
  endpoint?: string
  node?: string
}

export type FleetReplicationState = {
  mode: string
  sequence?: number | null
  health: string
  detail: string
}

export type FleetWorkload = {
  status: string
  source?: string
  active_tasks?: number | null
  task_ids?: string[]
  status_breakdown?: Record<string, number>
}

export type FleetNodeHardware = {
  discovered_at?: string
  last_seen?: string
  open_ports?: number[]
  cpu?: string
  ram?: string
  gpu?: string
  [key: string]: unknown
}

export type FleetNodeMetrics = {
  latency_ms?: number | null
  tcp_ok?: boolean | null
  http_ok?: boolean | null
  checked_at?: string | null
  active_tasks?: number | null
  [key: string]: unknown
}

export type FleetNode = {
  id?: string
  name: string
  hostname?: string
  ip?: string
  role?: string
  status?: NodeStatus | string
  health?: HealthState | string
  leader_state?: LeaderState | string
  is_leader?: boolean
  cpu?: string
  ram?: string
  gpu?: string
  models_loaded?: string[]
  models_loaded_state?: string
  source_kind?: string
  seeded_from_config?: boolean
  runtime_enrolled?: boolean
  runtime_provenance?: string[]
  last_heartbeat?: string
  heartbeat_source?: string
  heartbeat_freshness?: string
  heartbeat_age_seconds?: number | null
  service_version?: string
  replication_state?: FleetReplicationState
  current_workload?: FleetWorkload
  hardware?: FleetNodeHardware
  metrics?: FleetNodeMetrics
  models?: FleetModel[]
}

export type FleetStatusSummary = {
  total_nodes?: number
  connected_nodes?: number
  unhealthy_nodes?: number
  enrolled_nodes?: number
  seed_nodes?: number
  model_count?: number
  leader?: string
  gateway_version?: string
}

export type FleetStatusResponse = {
  status?: string
  total_nodes?: number
  summary?: FleetStatusSummary
  nodes?: FleetNode[]
  models?: FleetModel[]
  scanned_at?: string
  [key: string]: unknown
}

export type MissionCard = {
  id: string
  title: string
  owner?: string
  priority?: string
  status?: string
}

export type MissionColumn = {
  id: string
  title: string
  cards: MissionCard[]
}

export type WsEvent = {
  type: string
  timestamp: string
  payload?: unknown
}
