import type {
  FleetModel,
  FleetNode,
  FleetReplicationState,
  FleetStatusResponse,
  FleetStatusSummary,
  FleetWorkload,
  MissionColumn,
  WsEvent,
} from '../types'

function asRecord(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null
  return value as Record<string, unknown>
}

function asString(value: unknown, fallback = ''): string {
  return typeof value === 'string' ? value : fallback
}

function asNumber(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined
}

function asStringArray(value: unknown): string[] {
  if (!Array.isArray(value)) return []
  return value.filter((item): item is string => typeof item === 'string')
}

function normalizeModel(model: unknown, fallbackNode?: string): FleetModel | null {
  const record = asRecord(model)
  if (!record) return null

  const name = asString(record.name) || asString(record.id) || 'unknown-model'
  return {
    id: asString(record.id) || undefined,
    name,
    tier: asNumber(record.tier),
    status: asString(record.status) || undefined,
    contextWindow: asNumber(record.contextWindow),
    endpoint: asString(record.endpoint) || undefined,
    node: asString(record.node) || fallbackNode,
  }
}

function normalizeReplicationState(value: unknown): FleetReplicationState | undefined {
  const record = asRecord(value)
  if (!record) return undefined

  return {
    mode: asString(record.mode, 'unknown'),
    sequence: asNumber(record.sequence) ?? null,
    health: asString(record.health, 'unknown'),
    detail: asString(record.detail, 'unreported'),
  }
}

function normalizeWorkload(value: unknown): FleetWorkload | undefined {
  const record = asRecord(value)
  if (!record) return undefined

  const rawBreakdown = asRecord(record.status_breakdown) ?? {}
  const status_breakdown: Record<string, number> = {}
  for (const [status, count] of Object.entries(rawBreakdown)) {
    if (typeof count === 'number' && Number.isFinite(count)) {
      status_breakdown[status] = count
    }
  }

  return {
    status: asString(record.status, 'unreported'),
    source: asString(record.source) || undefined,
    active_tasks:
      typeof record.active_tasks === 'number' && Number.isFinite(record.active_tasks)
        ? record.active_tasks
        : null,
    task_ids: asStringArray(record.task_ids),
    status_breakdown,
  }
}

function normalizeNode(node: unknown): FleetNode | null {
  const record = asRecord(node)
  if (!record) return null

  const id = asString(record.id) || undefined
  const name = asString(record.name) || id || asString(record.ip) || 'unknown-node'

  const models = Array.isArray(record.models)
    ? (record.models
        .map((model) => normalizeModel(model, name))
        .filter((model): model is FleetModel => model !== null))
    : []

  const normalized: FleetNode = {
    id,
    name,
    hostname: asString(record.hostname) || undefined,
    ip: asString(record.ip) || undefined,
    role: asString(record.role) || undefined,
    status: asString(record.status, asString(record.health, 'unknown')).toLowerCase(),
    health: asString(record.health, asString(record.status, 'unknown')).toLowerCase(),
    leader_state: asString(record.leader_state) || undefined,
    is_leader: record.is_leader === true,
    cpu: asString(record.cpu) || undefined,
    ram: asString(record.ram) || undefined,
    gpu: asString(record.gpu) || undefined,
    models_loaded: asStringArray(record.models_loaded),
    models_loaded_state: asString(record.models_loaded_state) || undefined,
    source_kind: asString(record.source_kind) || undefined,
    seeded_from_config:
      typeof record.seeded_from_config === 'boolean' ? record.seeded_from_config : undefined,
    runtime_enrolled: typeof record.runtime_enrolled === 'boolean' ? record.runtime_enrolled : undefined,
    runtime_provenance: asStringArray(record.runtime_provenance),
    last_heartbeat: asString(record.last_heartbeat) || undefined,
    heartbeat_source: asString(record.heartbeat_source) || undefined,
    heartbeat_freshness: asString(record.heartbeat_freshness) || undefined,
    heartbeat_age_seconds:
      typeof record.heartbeat_age_seconds === 'number' && Number.isFinite(record.heartbeat_age_seconds)
        ? record.heartbeat_age_seconds
        : null,
    service_version: asString(record.service_version) || undefined,
    replication_state: normalizeReplicationState(record.replication_state),
    current_workload: normalizeWorkload(record.current_workload),
    hardware: (asRecord(record.hardware) ?? undefined) as FleetNode['hardware'],
    metrics: (asRecord(record.metrics) ?? undefined) as FleetNode['metrics'],
    models,
  }

  return normalized
}

export function extractNodes(payload: FleetStatusResponse | Record<string, unknown>): FleetNode[] {
  const root = asRecord(payload) ?? {}

  const directNodes = Array.isArray(root.nodes) ? root.nodes : []
  if (directNodes.length > 0) {
    return directNodes
      .map((node) => normalizeNode(node))
      .filter((node): node is FleetNode => node !== null)
  }

  const nested = asRecord(root.fleet)
  const nestedNodes = Array.isArray(nested?.nodes) ? nested.nodes : []
  return nestedNodes
    .map((node) => normalizeNode(node))
    .filter((node): node is FleetNode => node !== null)
}

export function extractSummary(payload: FleetStatusResponse | Record<string, unknown>): FleetStatusSummary {
  const root = asRecord(payload) ?? {}
  const summary = asRecord(root.summary)
  if (summary) {
    return {
      total_nodes: asNumber(summary.total_nodes),
      connected_nodes: asNumber(summary.connected_nodes),
      unhealthy_nodes: asNumber(summary.unhealthy_nodes),
      enrolled_nodes: asNumber(summary.enrolled_nodes),
      seed_nodes: asNumber(summary.seed_nodes),
      model_count: asNumber(summary.model_count),
      leader: asString(summary.leader) || undefined,
      gateway_version: asString(summary.gateway_version) || undefined,
    }
  }

  const nodes = extractNodes(payload)
  const connected_nodes = nodes.filter((node) => {
    const status = (node.status ?? node.health ?? 'unknown').toLowerCase()
    return status === 'online' || status === 'degraded' || status === 'healthy'
  }).length
  const unhealthy_nodes = nodes.filter((node) => {
    const status = (node.status ?? node.health ?? 'unknown').toLowerCase()
    return !(status === 'online' || status === 'healthy')
  }).length

  const modelIds = new Set<string>()
  for (const node of nodes) {
    for (const model of node.models_loaded ?? []) {
      modelIds.add(model)
    }
  }

  const enrolled_nodes = nodes.filter((node) => node.runtime_enrolled === true).length
  const seed_nodes = nodes.filter((node) => node.source_kind === 'seed/static').length

  return {
    total_nodes: nodes.length,
    connected_nodes,
    unhealthy_nodes,
    enrolled_nodes,
    seed_nodes,
    model_count: modelIds.size,
  }
}

export function extractModels(payload: FleetStatusResponse | Record<string, unknown>): FleetModel[] {
  const root = asRecord(payload) ?? {}

  if (Array.isArray(root.models)) {
    return root.models
      .map((model) => normalizeModel(model))
      .filter((model): model is FleetModel => model !== null)
  }

  const nested = asRecord(root.inventory)
  if (Array.isArray(nested?.models)) {
    return nested.models
      .map((model) => normalizeModel(model))
      .filter((model): model is FleetModel => model !== null)
  }

  const nodes = extractNodes(payload)
  return nodes.flatMap((node) =>
    (node.models ?? []).map((model) => ({ ...model, node: model.node ?? node.name })),
  )
}

export function parseBoard(payload: unknown): MissionColumn[] {
  if (!payload || typeof payload !== 'object') return []

  const direct = (payload as { columns?: unknown[] }).columns
  if (Array.isArray(direct)) {
    return direct.map((column, idx) => {
      if (typeof column !== 'object' || column === null) {
        return { id: `${idx}`, title: `Column ${idx + 1}`, cards: [] }
      }

      const c = column as {
        id?: string
        title?: string
        label?: string
        cards?: unknown[]
        items?: unknown[]
      }

      const cardsRaw = Array.isArray(c.cards) ? c.cards : Array.isArray(c.items) ? c.items : []
      const cards = cardsRaw.map((card, cardIdx) => {
        if (typeof card === 'object' && card !== null) {
          const obj = card as {
            id?: string
            title?: string
            assignee?: string
            owner?: string
            priority?: string | number
            status?: string
          }
          return {
            id: String(obj.id ?? `${idx}-${cardIdx}`),
            title: String(obj.title ?? 'Untitled mission'),
            owner: obj.owner ?? obj.assignee,
            priority:
              typeof obj.priority === 'number'
                ? String(obj.priority)
                : obj.priority,
            status: obj.status,
          }
        }
        return { id: `${idx}-${cardIdx}`, title: String(card) }
      })

      return {
        id: c.id ?? `${idx}-${c.title ?? c.label ?? 'column'}`,
        title: String(c.title ?? c.label ?? `Column ${idx + 1}`),
        cards,
      }
    })
  }

  const fromObject = (payload as { board?: Record<string, unknown[]> }).board
  if (fromObject && typeof fromObject === 'object') {
    return Object.entries(fromObject).map(([title, cards], idx) => ({
      id: `${idx}-${title}`,
      title,
      cards: Array.isArray(cards)
        ? cards.map((card, cardIdx) => {
            if (typeof card === 'object' && card !== null) {
              return {
                id: String((card as { id?: string }).id ?? `${idx}-${cardIdx}`),
                title: String((card as { title?: string }).title ?? 'Untitled mission'),
                owner: (card as { owner?: string }).owner,
                priority: (card as { priority?: string }).priority,
                status: (card as { status?: string }).status,
              }
            }
            return { id: `${idx}-${cardIdx}`, title: String(card) }
          })
        : [],
    }))
  }

  return []
}

export function parseWsMessage(raw: string): WsEvent {
  try {
    const parsed = JSON.parse(raw) as Partial<WsEvent>
    return {
      type: parsed.type ?? 'message',
      timestamp: parsed.timestamp ?? new Date().toISOString(),
      payload: parsed.payload,
    }
  } catch {
    return {
      type: 'message',
      timestamp: new Date().toISOString(),
      payload: raw,
    }
  }
}
