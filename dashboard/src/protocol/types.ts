// Shared dashboard contract consumed by both web dashboard and TUI.
// In the future these types will be generated from crates/ff-dashboard-core.

export interface FleetNode {
  id: string
  name: string
  ip: string
  os: string
  role: 'leader' | 'standby' | 'worker' | 'gateway' | string
  daemonOnline: boolean
  models: NodeModel[]
  gpuHistory: number[]
  cpuPercent?: number
  ramPercent?: number
}

export interface NodeModel {
  name: string
  runtime: string
  port: number
  online: boolean
  contextWindow: number
  tokensUsed: number
}

export interface LlmServer {
  id: string
  model: string
  runtime: string
  host: string
  port: number
  online: boolean
  tier: number
}

export type WorkItemStatus =
  | 'ready'
  | 'building'
  | 'in_review'
  | 'done'
  | 'failed'
  | 'blocked'
  | 'deferred'

export interface WorkItem {
  id: string
  title: string
  description?: string
  status: WorkItemStatus
  priority: number
  assignee?: string
  host?: string
  project?: string
  epic?: string
  sprint?: string
  createdAt: string
  updatedAt: string
}

export interface AgentRun {
  id: string
  sessionId: string
  status: 'idle' | 'running' | 'cancelled' | 'done' | 'error'
  model: string
  backend: string
  turn: number
  elapsedSeconds?: number
  lastActivity: string
}

export interface SessionSummary {
  id: string
  name: string
  project?: string
  messageCount: number
  lastActive: string
}

export interface Alert {
  id: string
  severity: 'info' | 'warning' | 'critical'
  message: string
  source: string
  firedAt: string
  clearedAt?: string
}

export interface Skill {
  id: string
  name: string
  description: string
  scope: 'project' | 'user' | 'extra' | 'built-in'
  path?: string
}

export interface CapabilityManifest {
  routes: RouteDef[]
  commands: CommandDef[]
}

export interface RouteDef {
  id: string
  path: string
  label: string
  icon?: string
  parent?: string
}

export interface CommandDef {
  id: string
  label: string
  shortcut?: string
  scope?: 'global' | 'table' | 'modal' | 'form'
  route?: string
  action?: string
}

export interface OperatorContext {
  nodeName: string
  gatewayUrl: string
  version: string
}

export interface DashboardSnapshot {
  fleet: FleetNode[]
  llmServers: LlmServer[]
  workItems: WorkItem[]
  agents: AgentRun[]
  sessions: SessionSummary[]
  alerts: Alert[]
  skills: Skill[]
  capabilities: CapabilityManifest
  me: OperatorContext
}

export type DashboardEvent =
  | { kind: 'FleetUpdated'; seq: number; node: FleetNode }
  | { kind: 'LlmServerUpdated'; seq: number; server: LlmServer }
  | { kind: 'WorkItemChanged'; seq: number; item: WorkItem }
  | { kind: 'AgentEvent'; seq: number; runId: string; event: AgentRunEvent }
  | { kind: 'AlertFired'; seq: number; alert: Alert }
  | { kind: 'AlertCleared'; seq: number; alertId: string }
  | { kind: 'Heartbeat'; seq: number }
  | { kind: 'ResyncRequired'; seq: number }

export type AgentRunEvent =
  | { kind: 'TurnComplete'; turn: number }
  | { kind: 'TokenWarning'; usagePct: number; estimatedTokens: number }
  | { kind: 'Done' }
  | { kind: 'Error'; message: string }
  | { kind: 'Status'; message: string }
  | { kind: 'ToolStart'; toolName: string }
  | { kind: 'ToolEnd'; toolName: string; isError: boolean; durationMs: number }
  | { kind: 'AssistantText' }
  | { kind: 'Compaction'; messagesBefore: number; messagesAfter: number }
