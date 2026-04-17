import { useCallback, useEffect, useRef, useState } from 'react'
import { getJson } from '../lib/api'

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

function initSimNodes(
  nodes: GraphNode[],
  communities: Community[],
  width: number,
  height: number,
): SimNode[] {
  const colorMap = new Map<number, string>()
  for (const c of communities) colorMap.set(c.id, c.color)

  const fallbackColors = [
    '#6366f1', '#8b5cf6', '#a855f7', '#d946ef', '#ec4899',
    '#f43f5e', '#f97316', '#eab308', '#22c55e', '#14b8a6',
  ]

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
      let dx = a.x - b.x
      let dy = a.y - b.y
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
    let dx = b.x - a.x
    let dy = b.y - a.y
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
    ctx.strokeStyle = 'rgba(100,116,139,0.25)'

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
    ctx.fillStyle = 'rgba(148,163,184,0.3)'
    ctx.fillText(comm.label, c.x / c.count, c.y / c.count - 20)
  }

  // Draw nodes
  for (const n of simNodes) {
    const isHovered = n.id === hoveredId
    const isSearchHit = searchHits.size > 0 && searchHits.has(n.id)
    const dimmed = searchHits.size > 0 && !isSearchHit && !isHovered

    ctx.beginPath()
    ctx.arc(n.x, n.y, n.radius, 0, Math.PI * 2)
    ctx.fillStyle = dimmed ? 'rgba(51,65,85,0.3)' : n.color
    ctx.fill()

    if (isHovered || isSearchHit) {
      ctx.strokeStyle = isHovered ? '#e2e8f0' : '#a78bfa'
      ctx.lineWidth = 2
      ctx.stroke()
    }

    // Label
    if (isHovered || n.radius > 8) {
      ctx.fillStyle = dimmed ? 'rgba(148,163,184,0.2)' : 'rgba(226,232,240,0.85)'
      ctx.font = isHovered ? 'bold 11px system-ui' : '10px system-ui'
      ctx.textAlign = 'center'
      ctx.fillText(n.title.slice(0, 30), n.x, n.y - n.radius - 4)
    }
  }
}

/* ------------------------------------------------------------------ */
/*  Detail Panel                                                       */
/* ------------------------------------------------------------------ */

function DetailPanel({
  node,
  communities,
  onClose,
}: {
  node: SimNode
  communities: Community[]
  onClose: () => void
}) {
  const community = communities.find((c) => c.id === node.community_id)

  const openInObsidian = () => {
    window.open(
      'obsidian://open?vault=Yarli_KnowledgeBase&file=' + encodeURIComponent(node.path),
    )
  }

  return (
    <div className="absolute right-4 top-4 z-20 w-72 rounded-lg border border-slate-700 bg-slate-900/95 p-4 shadow-xl backdrop-blur">
      <div className="mb-3 flex items-start justify-between">
        <h3 className="text-sm font-semibold text-slate-200">{node.title}</h3>
        <button onClick={onClose} className="text-slate-500 hover:text-slate-300">
          ✕
        </button>
      </div>
      <div className="space-y-1.5 text-xs font-mono">
        <div className="flex gap-2">
          <span className="text-slate-500">type</span>
          <span className="text-slate-300">{node.node_type}</span>
        </div>
        <div className="flex gap-2">
          <span className="text-slate-500">path</span>
          <span className="text-slate-300 break-all">{node.path}</span>
        </div>
        <div className="flex gap-2">
          <span className="text-slate-500">community</span>
          <span className="text-slate-300">
            {community ? (
              <>
                <span
                  className="mr-1 inline-block h-2 w-2 rounded-full"
                  style={{ backgroundColor: community.color }}
                />
                {community.label}
              </>
            ) : (
              '—'
            )}
          </span>
        </div>
        <div className="flex gap-2">
          <span className="text-slate-500">hits</span>
          <span className="text-slate-300">{node.hits}</span>
        </div>
      </div>
      <button
        onClick={openInObsidian}
        className="mt-3 w-full rounded border border-slate-700 px-3 py-1.5 text-xs text-slate-300 transition hover:bg-slate-800"
      >
        Open in Obsidian
      </button>
    </div>
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

  if (loading) return <div className="p-6 text-slate-400">Loading knowledge graph...</div>
  if (error) {
    return (
      <div className="p-6">
        <div className="text-rose-400">Could not load brain graph: {error}</div>
        <div className="mt-2 text-sm text-slate-500">
          The Brain graph API is not running yet. Start the daemon or check logs.
        </div>
      </div>
    )
  }

  return (
    <div className="flex h-full flex-col -m-4 md:-m-6">
      {/* Top bar */}
      <div className="flex items-center gap-3 border-b border-slate-800 px-4 py-2">
        <h2 className="text-sm font-semibold text-slate-200">Knowledge Graph</h2>
        <input
          type="text"
          value={searchQuery}
          onChange={(e) => setSearchQuery(e.target.value)}
          placeholder="Search nodes..."
          className="w-64 rounded border border-slate-700 bg-slate-900 px-2 py-1 text-xs text-slate-200 placeholder-slate-500 outline-none focus:border-violet-500"
        />
        <span className="text-[10px] text-slate-600">
          {graphData?.nodes.length ?? 0} nodes · {graphData?.edges.length ?? 0} edges
        </span>
      </div>

      {/* Canvas */}
      <div ref={containerRef} className="relative flex-1">
        <canvas
          ref={canvasRef}
          onMouseMove={onCanvasMouseMove}
          onClick={onCanvasClick}
          className="block h-full w-full"
        />
        {selectedNode && (
          <DetailPanel
            node={selectedNode}
            communities={communities}
            onClose={() => setSelectedNode(null)}
          />
        )}
      </div>
    </div>
  )
}
