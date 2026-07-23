import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { Badge } from '../components/ui/badge'
import { StatusBadge } from '../components/ui/status-badge'
import { Button } from '../components/ui/button'
import { getJson } from '../lib/api'
import { cn } from '../lib/utils'

/* ------------------------------------------------------------------ */
/*  Types                                                              */
/* ------------------------------------------------------------------ */

interface GraphNode {
  id: string
  path: string
  title: string
  node_type: string
  community_id: number | null
  hits: number
}

interface GraphEdge {
  src_id: string
  dst_id: string
  edge_type: 'extends' | 'link' | 'overrides' | string
  confidence: number
}

interface Community {
  id: number
  label: string
  color: string
}

interface GraphResp {
  nodes: GraphNode[]
  edges: GraphEdge[]
  communities: Community[]
}

/* Simulation node with position + velocity */
interface SimNode extends GraphNode {
  x: number
  y: number
  vx: number
  vy: number
  radius: number
  color: string
}

/* ------------------------------------------------------------------ */
/*  Force simulation helpers                                           */
/* ------------------------------------------------------------------ */

const REPULSION = 800
const SPRING_K = 0.005
const SPRING_REST = 80
const DAMPING = 0.92
const CENTER_PULL = 0.01
const DT = 1

const fallbackColors = [
  '#6366f1', '#8b5cf6', '#a855f7', '#d946ef', '#ec4899',
  '#f43f5e', '#f97316', '#eab308', '#22c55e', '#14b8a6',
]

const tokenFallbacks = {
  '--color-border-subtle': '#3f3f46',
  '--color-elevated': '#18181b',
  '--color-foreground': '#fafafa',
  '--color-muted': '#a1a1aa',
  '--color-primary-muted': '#a78bfa',
} as const

type ColorToken = keyof typeof tokenFallbacks

function tokenColor(token: ColorToken): string {
  if (typeof window === 'undefined') return tokenFallbacks[token]
  return (
    getComputedStyle(document.documentElement).getPropertyValue(token).trim() ||
    tokenFallbacks[token]
  )
}

function tokenRgba(token: ColorToken, alpha: number): string {
  const hex = tokenColor(token).replace('#', '')
  const normalized =
    hex.length === 3
      ? hex.split('').map((part) => part + part).join('')
      : hex
  const value = Number.parseInt(normalized, 16)
  if (Number.isNaN(value)) return tokenColor(token)
  const r = (value >> 16) & 255
  const g = (value >> 8) & 255
  const b = value & 255
  return `rgba(${r},${g},${b},${alpha})`
}

function initSimNodes(
  nodes: GraphNode[],
  communities: Community[],
  width: number,
  height: number,
): SimNode[] {
  const colorMap = new Map<number, string>()
  for (const c of communities) colorMap.set(c.id, c.color)

  return nodes.map((n, i) => {
    const angle = (i / nodes.length) * Math.PI * 2
    const r = Math.min(width, height) * 0.3
    const communityColor =
      n.community_id != null ? colorMap.get(n.community_id) : undefined
    return {
      ...n,
      x: width / 2 + Math.cos(angle) * r + (Math.random() - 0.5) * 40,
      y: height / 2 + Math.sin(angle) * r + (Math.random() - 0.5) * 40,
      vx: 0,
      vy: 0,
      radius: Math.max(4, Math.min(20, 4 + Math.sqrt(n.hits) * 2)),
      color: communityColor || fallbackColors[i % fallbackColors.length],
    }
  })
}

function stepSimulation(
  simNodes: SimNode[],
  edges: GraphEdge[],
  width: number,
  height: number,
): void {
  const nodeMap = new Map<string, SimNode>()
  for (const n of simNodes) nodeMap.set(n.id, n)

  // Repulsion (Coulomb)
  for (let i = 0; i < simNodes.length; i++) {
    for (let j = i + 1; j < simNodes.length; j++) {
      const a = simNodes[i]
      const b = simNodes[j]
      const dx = a.x - b.x
      const dy = a.y - b.y
      let dist = Math.sqrt(dx * dx + dy * dy)
      if (dist < 1) dist = 1
      const force = REPULSION / (dist * dist)
      const fx = (dx / dist) * force
      const fy = (dy / dist) * force
      a.vx += fx * DT
      a.vy += fy * DT
      b.vx -= fx * DT
      b.vy -= fy * DT
    }
  }

  // Spring (edges)
  for (const e of edges) {
    const a = nodeMap.get(e.src_id)
    const b = nodeMap.get(e.dst_id)
    if (!a || !b) continue
    const dx = b.x - a.x
    const dy = b.y - a.y
    let dist = Math.sqrt(dx * dx + dy * dy)
    if (dist < 1) dist = 1
    const displacement = dist - SPRING_REST
    const force = SPRING_K * displacement
    const fx = (dx / dist) * force
    const fy = (dy / dist) * force
    a.vx += fx * DT
    a.vy += fy * DT
    b.vx -= fx * DT
    b.vy -= fy * DT
  }

  // Center pull + damping + integrate
  const cx = width / 2
  const cy = height / 2
  for (const n of simNodes) {
    n.vx += (cx - n.x) * CENTER_PULL
    n.vy += (cy - n.y) * CENTER_PULL
    n.vx *= DAMPING
    n.vy *= DAMPING
    n.x += n.vx * DT
    n.y += n.vy * DT
    // Clamp to bounds
    n.x = Math.max(n.radius, Math.min(width - n.radius, n.x))
    n.y = Math.max(n.radius, Math.min(height - n.radius, n.y))
  }
}

/* ------------------------------------------------------------------ */
/*  Canvas renderer                                                    */
/* ------------------------------------------------------------------ */

function drawGraph(
  ctx: CanvasRenderingContext2D,
  simNodes: SimNode[],
  edges: GraphEdge[],
  communities: Community[],
  width: number,
  height: number,
  hoveredId: string | null,
  searchHits: Set<string>,
) {
  const nodeMap = new Map<string, SimNode>()
  for (const n of simNodes) nodeMap.set(n.id, n)

  ctx.clearRect(0, 0, width, height)

  // Draw edges
  for (const e of edges) {
    const a = nodeMap.get(e.src_id)
    const b = nodeMap.get(e.dst_id)
    if (!a || !b) continue

    ctx.beginPath()
    ctx.moveTo(a.x, a.y)
    ctx.lineTo(b.x, b.y)
    ctx.lineWidth = Math.max(0.5, e.confidence * 2)
    ctx.strokeStyle = tokenRgba('--color-border-subtle', 0.3)

    if (e.edge_type === 'extends') {
      ctx.setLineDash([6, 4])
    } else if (e.edge_type === 'overrides') {
      ctx.setLineDash([2, 3])
    } else {
      ctx.setLineDash([])
    }
    ctx.stroke()
    ctx.setLineDash([])
  }

  // Draw community labels at cluster centers
  const commCenters = new Map<number, { x: number; y: number; count: number }>()
  for (const n of simNodes) {
    if (n.community_id == null) continue
    const c = commCenters.get(n.community_id) || { x: 0, y: 0, count: 0 }
    c.x += n.x
    c.y += n.y
    c.count++
    commCenters.set(n.community_id, c)
  }
  ctx.font = '10px system-ui'
  ctx.textAlign = 'center'
  for (const comm of communities) {
    const c = commCenters.get(comm.id)
    if (!c || c.count === 0) continue
    ctx.fillStyle = tokenRgba('--color-muted', 0.32)
    ctx.fillText(comm.label, c.x / c.count, c.y / c.count - 20)
  }

  // Draw nodes
  for (const n of simNodes) {
    const isHovered = n.id === hoveredId
    const isSearchHit = searchHits.size > 0 && searchHits.has(n.id)
    const dimmed = searchHits.size > 0 && !isSearchHit && !isHovered

    ctx.beginPath()
    ctx.arc(n.x, n.y, n.radius, 0, Math.PI * 2)
    ctx.fillStyle = dimmed ? tokenRgba('--color-elevated', 0.55) : n.color
    ctx.fill()

    if (isHovered || isSearchHit) {
      ctx.strokeStyle = isHovered
        ? tokenColor('--color-foreground')
        : tokenColor('--color-primary-muted')
      ctx.lineWidth = 2
      ctx.stroke()
    }

    // Label
    if (isHovered || n.radius > 8) {
      ctx.fillStyle = dimmed
        ? tokenRgba('--color-muted', 0.25)
        : tokenRgba('--color-foreground', 0.85)
      ctx.font = isHovered ? 'bold 11px system-ui' : '10px system-ui'
      ctx.textAlign = 'center'
      ctx.fillText(n.title.slice(0, 30), n.x, n.y - n.radius - 4)
    }
  }
}

/* ------------------------------------------------------------------ */
/*  Detail Panel                                                       */
/* ------------------------------------------------------------------ */

type BadgeTone = 'default' | 'ok' | 'warn' | 'crit' | 'info' | 'neutral'

function edgeTone(edgeType: string): BadgeTone {
  if (edgeType === 'extends') return 'info'
  if (edgeType === 'overrides') return 'warn'
  if (edgeType === 'link') return 'neutral'
  return 'default'
}

function DetailRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="grid grid-cols-[5.5rem_minmax(0,1fr)] gap-3 text-xs">
      <dt className="font-mono uppercase text-dim">{label}</dt>
      <dd className="min-w-0 font-mono text-muted">{children}</dd>
    </div>
  )
}

function DetailPanel({
  node,
  nodes,
  edges,
  communities,
  onClose,
}: {
  node: SimNode
  nodes: GraphNode[]
  edges: GraphEdge[]
  communities: Community[]
  onClose: () => void
}) {
  const community = communities.find((c) => c.id === node.community_id)
  const nodeById = new Map(nodes.map((item) => [item.id, item]))
  const connectedEdges = edges.filter((edge) => edge.src_id === node.id || edge.dst_id === node.id)

  const openInObsidian = () => {
    window.open(
      'obsidian://open?vault=Yarli_KnowledgeBase&file=' + encodeURIComponent(node.path),
    )
  }

  return (
    <Card className="absolute inset-x-3 top-3 z-20 max-h-[calc(100%-1.5rem)] overflow-y-auto border-border-subtle bg-panel/95 shadow-xl backdrop-blur-sm sm:left-auto sm:right-4 sm:top-4 sm:w-96">
      <CardHeader className="mb-4 items-start gap-3">
        <div className="min-w-0">
          <CardTitle className="truncate">{node.title}</CardTitle>
          <CardDescription className="mt-1 break-all">{node.id}</CardDescription>
        </div>
        <Button variant="ghost" size="sm" onClick={onClose} aria-label="Close node details">
          Close
        </Button>
      </CardHeader>

      <div className="space-y-2">
        <DetailRow label="type">
          <StatusBadge status={node.node_type}>{node.node_type}</StatusBadge>
        </DetailRow>
        <DetailRow label="path">
          <span className="break-all">{node.path}</span>
        </DetailRow>
        <DetailRow label="community">
          {community ? (
            <span className="inline-flex min-w-0 items-center gap-2 text-muted">
              <span
                className="h-2 w-2 shrink-0 rounded-full"
                style={{ backgroundColor: community.color }}
              />
              <span className="truncate">{community.label}</span>
            </span>
          ) : (
            <span className="text-dim">-</span>
          )}
        </DetailRow>
        <DetailRow label="hits">{node.hits}</DetailRow>
      </div>

      <div className="mt-4 border-t border-border pt-4">
        <div className="mb-2 flex items-center justify-between gap-3">
          <CardDescription className="font-mono uppercase tracking-wide">Edges</CardDescription>
          <Badge variant="neutral">{connectedEdges.length}</Badge>
        </div>
        {connectedEdges.length === 0 ? (
          <p className="text-xs text-dim">No connected edges.</p>
        ) : (
          <div className="space-y-2">
            {connectedEdges.slice(0, 8).map((edge) => {
              const isOutgoing = edge.src_id === node.id
              const peer = nodeById.get(isOutgoing ? edge.dst_id : edge.src_id)
              return (
                <div
                  key={`${edge.src_id}-${edge.dst_id}-${edge.edge_type}`}
                  className="rounded-lg border border-border bg-surface px-3 py-2"
                >
                  <div className="flex items-center justify-between gap-2">
                    <Badge variant={edgeTone(edge.edge_type)}>{edge.edge_type}</Badge>
                    <span className="font-mono text-xs text-dim">
                      {Math.round(edge.confidence * 100)}%
                    </span>
                  </div>
                  <div className="mt-2 min-w-0 text-xs text-muted">
                    <span className="font-mono text-dim">{isOutgoing ? 'to' : 'from'}</span>{' '}
                    <span className="break-all text-foreground">{peer?.title ?? (isOutgoing ? edge.dst_id : edge.src_id)}</span>
                  </div>
                </div>
              )
            })}
            {connectedEdges.length > 8 ? (
              <p className="text-xs text-dim">{connectedEdges.length - 8} more edges not shown</p>
            ) : null}
          </div>
        )}
      </div>

      <Button onClick={openInObsidian} variant="outline" className="mt-4 w-full">
        Open in Obsidian
      </Button>
    </Card>
  )
}

/* ------------------------------------------------------------------ */
/*  Main BrainGraph Page                                               */
/* ------------------------------------------------------------------ */

export function BrainGraph() {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const containerRef = useRef<HTMLDivElement>(null)

  const [graphData, setGraphData] = useState<GraphResp | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const [simNodes, setSimNodes] = useState<SimNode[]>([])
  const [_edges, setEdges] = useState<GraphEdge[]>([])
  const [communities, setCommunities] = useState<Community[]>([])

  const [selectedNode, setSelectedNode] = useState<SimNode | null>(null)
  const [hoveredId, setHoveredId] = useState<string | null>(null)
  const [searchQuery, setSearchQuery] = useState('')

  const simNodesRef = useRef<SimNode[]>([])
  const edgesRef = useRef<GraphEdge[]>([])
  const communitiesRef = useRef<Community[]>([])
  const hoveredIdRef = useRef<string | null>(null)
  const searchHitsRef = useRef<Set<string>>(new Set())

  // Fetch data
  useEffect(() => {
    let cancelled = false
    async function load() {
      try {
        const data = await getJson<GraphResp>('/api/brain/graph')
        if (cancelled) return
        setGraphData(data)
        setLoading(false)
      } catch (e) {
        if (cancelled) return
        setError(e instanceof Error ? e.message : String(e))
        setLoading(false)
      }
    }
    load()
    return () => { cancelled = true }
  }, [])

  // Initialize simulation when data arrives
  useEffect(() => {
    if (!graphData || !containerRef.current) return
    const rect = containerRef.current.getBoundingClientRect()
    const w = rect.width || 800
    const h = rect.height || 600
    const sn = initSimNodes(graphData.nodes, graphData.communities, w, h)
    setSimNodes(sn)
    setEdges(graphData.edges)
    setCommunities(graphData.communities)
    simNodesRef.current = sn
    edgesRef.current = graphData.edges
    communitiesRef.current = graphData.communities
  }, [graphData])

  // Search hits
  useEffect(() => {
    if (!searchQuery) {
      searchHitsRef.current = new Set()
      return
    }
    const lq = searchQuery.toLowerCase()
    const hits = new Set<string>()
    for (const n of simNodesRef.current) {
      if (
        n.title.toLowerCase().includes(lq) ||
        n.path.toLowerCase().includes(lq) ||
        n.node_type.toLowerCase().includes(lq)
      ) {
        hits.add(n.id)
      }
    }
    searchHitsRef.current = hits
  }, [searchQuery, simNodes])

  // Keep refs in sync
  useEffect(() => {
    hoveredIdRef.current = hoveredId
  }, [hoveredId])

  // Animation loop
  useEffect(() => {
    if (simNodesRef.current.length === 0) return
    const canvas = canvasRef.current
    if (!canvas) return
    const ctx = canvas.getContext('2d')
    if (!ctx) return

    let running = true
    let frame = 0

    function tick() {
      if (!running || !ctx || !canvas) return
      const w = canvas.width
      const h = canvas.height
      // Run fewer simulation steps after stabilization
      if (frame < 300) {
        stepSimulation(simNodesRef.current, edgesRef.current, w, h)
      } else if (frame % 4 === 0) {
        stepSimulation(simNodesRef.current, edgesRef.current, w, h)
      }
      drawGraph(
        ctx,
        simNodesRef.current,
        edgesRef.current,
        communitiesRef.current,
        w,
        h,
        hoveredIdRef.current,
        searchHitsRef.current,
      )
      frame++
      requestAnimationFrame(tick)
    }
    requestAnimationFrame(tick)
    return () => { running = false }
  }, [simNodes])

  // Resize handler
  useEffect(() => {
    const container = containerRef.current
    const canvas = canvasRef.current
    if (!container || !canvas) return

    const observer = new ResizeObserver(() => {
      const rect = container.getBoundingClientRect()
      canvas.width = rect.width
      canvas.height = rect.height
    })
    observer.observe(container)
    // Initial size
    const rect = container.getBoundingClientRect()
    canvas.width = rect.width
    canvas.height = rect.height
    return () => observer.disconnect()
  }, [])

  // Mouse interaction
  const findNodeAt = useCallback(
    (cx: number, cy: number): SimNode | null => {
      for (let i = simNodesRef.current.length - 1; i >= 0; i--) {
        const n = simNodesRef.current[i]
        const dx = n.x - cx
        const dy = n.y - cy
        if (dx * dx + dy * dy <= (n.radius + 4) * (n.radius + 4)) {
          return n
        }
      }
      return null
    },
    [],
  )

  const onCanvasMouseMove = useCallback(
    (e: React.MouseEvent<HTMLCanvasElement>) => {
      const canvas = canvasRef.current
      if (!canvas) return
      const rect = canvas.getBoundingClientRect()
      const x = e.clientX - rect.left
      const y = e.clientY - rect.top
      const node = findNodeAt(x, y)
      setHoveredId(node?.id ?? null)
      canvas.style.cursor = node ? 'pointer' : 'default'
    },
    [findNodeAt],
  )

  const onCanvasClick = useCallback(
    (e: React.MouseEvent<HTMLCanvasElement>) => {
      const canvas = canvasRef.current
      if (!canvas) return
      const rect = canvas.getBoundingClientRect()
      const x = e.clientX - rect.left
      const y = e.clientY - rect.top
      const node = findNodeAt(x, y)
      setSelectedNode(node)
    },
    [findNodeAt],
  )

  const nodeCount = graphData?.nodes.length ?? 0
  const edgeCount = graphData?.edges.length ?? 0
  const communityCount = graphData?.communities.length ?? 0
  const searchTerm = searchQuery.trim().toLowerCase()
  const searchMatchCount = searchTerm
    ? simNodes.filter((node) =>
        node.title.toLowerCase().includes(searchTerm) ||
        node.path.toLowerCase().includes(searchTerm) ||
        node.node_type.toLowerCase().includes(searchTerm)
      ).length
    : null

  if (loading) {
    return (
      <section className="min-h-full bg-background p-6 text-foreground">
        <Card className="bg-panel">
          <CardHeader className="mb-0 gap-3">
            <div>
              <CardTitle>Knowledge Graph</CardTitle>
              <CardDescription>Loading graph data from /api/brain/graph</CardDescription>
            </div>
            <StatusBadge status="running">loading</StatusBadge>
          </CardHeader>
          <div className="mt-4 h-80 rounded-lg border border-border bg-surface" />
        </Card>
      </section>
    )
  }

  if (error) {
    return (
      <section className="min-h-full bg-background p-6 text-foreground">
        <Card className="border-status-crit bg-panel">
          <CardHeader className="mb-0 gap-3">
            <div>
              <CardTitle className="text-status-crit">Could not load brain graph</CardTitle>
              <CardDescription>{error}</CardDescription>
            </div>
            <StatusBadge status="error">error</StatusBadge>
          </CardHeader>
          <p className="mt-4 text-sm text-muted">
            The Brain graph API is not running yet. Start the daemon or check logs.
          </p>
        </Card>
      </section>
    )
  }

  return (
    <section className="-m-4 flex h-full min-h-[640px] flex-col bg-background text-foreground md:-m-6">
      <div className="border-b border-border bg-surface px-4 py-4 md:px-6">
        <div className="flex flex-col gap-4 lg:flex-row lg:items-center lg:justify-between">
          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-2">
              <h1 className="text-2xl font-bold text-foreground">Knowledge Graph</h1>
              <StatusBadge status={error ? 'error' : 'active'}>{error ? 'error' : 'active'}</StatusBadge>
            </div>
            <p className="mt-1 text-sm text-dim">Brain nodes, semantic links, and community clusters.</p>
          </div>

          <div className="flex flex-col gap-3 sm:flex-row sm:items-center">
            <label className="relative block sm:w-80">
              <span className="sr-only">Search nodes</span>
              <input
                type="text"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                placeholder="Search nodes..."
                className={cn(
                  'h-9 w-full rounded-lg border bg-panel px-3 text-sm text-foreground outline-hidden transition placeholder:text-dim focus:border-primary',
                  searchQuery ? 'border-border-subtle' : 'border-border'
                )}
              />
            </label>
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant="neutral">{nodeCount} nodes</Badge>
              <Badge variant="neutral">{edgeCount} edges</Badge>
              <Badge variant="neutral">{communityCount} communities</Badge>
              {searchMatchCount != null ? (
                <Badge variant={searchMatchCount > 0 ? 'info' : 'warn'}>
                  {searchMatchCount} matches
                </Badge>
              ) : null}
            </div>
          </div>
        </div>
      </div>

      <Card className="m-3 flex min-h-0 flex-1 flex-col overflow-hidden bg-surface p-0 md:m-4">
        <CardHeader className="mb-0 border-b border-border px-4 py-3">
          <div>
            <CardTitle>Graph Visualization</CardTitle>
            <CardDescription>Select a node to inspect metadata and connected edges.</CardDescription>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Badge variant="info">extends</Badge>
            <Badge variant="warn">overrides</Badge>
            <Badge variant="neutral">link</Badge>
          </div>
        </CardHeader>

        <div
          ref={containerRef}
          className="relative min-h-[520px] flex-1 bg-background bg-[radial-gradient(circle_at_1px_1px,var(--color-border)_1px,transparent_0)] bg-size-[24px_24px]"
        >
          <canvas
            ref={canvasRef}
            onMouseMove={onCanvasMouseMove}
            onClick={onCanvasClick}
            className="block h-full w-full"
          />
          {selectedNode ? (
            <DetailPanel
              node={selectedNode}
              nodes={graphData?.nodes ?? []}
              edges={graphData?.edges ?? []}
              communities={communities}
              onClose={() => setSelectedNode(null)}
            />
          ) : null}
        </div>
      </Card>
    </section>
  )
}
