import { useEffect, useMemo, useRef, useState, useCallback } from 'react'
import type { DashboardEvent } from '../protocol/types'

const INITIAL_RECONNECT_MS = 1000
const MAX_RECONNECT_MS = 30000
const BACKOFF_MULTIPLIER = 2
const HEARTBEAT_INTERVAL_MS = 15000

export type WsMessageHandler = (event: DashboardEvent) => void

export interface WsClientState {
  connected: boolean
  eventCount: number
  lastEvent: DashboardEvent | null
}

export function useDashboardWebSocket(path = '/ws', onEvent?: WsMessageHandler): WsClientState {
  const [connected, setConnected] = useState(false)
  const [eventCount, setEventCount] = useState(0)
  const [lastEvent, setLastEvent] = useState<DashboardEvent | null>(null)
  const reconnectTimer = useRef<number | null>(null)
  const reconnectDelay = useRef(INITIAL_RECONNECT_MS)
  const heartbeatTimer = useRef<number | null>(null)
  const handlerRef = useRef(onEvent)
  useEffect(() => {
    handlerRef.current = onEvent
  })

  const wsUrl = useMemo(() => {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    return `${protocol}//${window.location.host}${path}`
  }, [path])

  const handleMessage = useCallback((raw: string) => {
    let event: DashboardEvent
    try {
      const parsed = JSON.parse(raw) as Partial<DashboardEvent>
      event = normalizeEvent(parsed)
    } catch {
      event = { kind: 'Heartbeat', seq: 0 }
    }
    setEventCount((c) => c + 1)
    setLastEvent(event)
    handlerRef.current?.(event)
  }, [])

  useEffect(() => {
    let socket: WebSocket | null = null
    let alive = true

    const clearHeartbeat = () => {
      if (heartbeatTimer.current) {
        window.clearInterval(heartbeatTimer.current)
        heartbeatTimer.current = null
      }
    }

    const connect = () => {
      socket = new WebSocket(wsUrl)

      socket.onopen = () => {
        setConnected(true)
        reconnectDelay.current = INITIAL_RECONNECT_MS
        heartbeatTimer.current = window.setInterval(() => {
          if (socket?.readyState === WebSocket.OPEN) {
            socket.send(JSON.stringify({ kind: 'Ping' }))
          }
        }, HEARTBEAT_INTERVAL_MS)
      }

      socket.onclose = () => {
        setConnected(false)
        clearHeartbeat()
        if (alive) {
          reconnectTimer.current = window.setTimeout(() => {
            reconnectDelay.current = Math.min(
              reconnectDelay.current * BACKOFF_MULTIPLIER,
              MAX_RECONNECT_MS
            )
            connect()
          }, reconnectDelay.current)
        }
      }

      socket.onerror = () => setConnected(false)
      socket.onmessage = (ev) => handleMessage(String(ev.data))
    }

    connect()

    return () => {
      alive = false
      if (reconnectTimer.current) window.clearTimeout(reconnectTimer.current)
      clearHeartbeat()
      socket?.close()
    }
  }, [wsUrl, handleMessage])

  return { connected, eventCount, lastEvent }
}

function normalizeEvent(parsed: Partial<DashboardEvent>): DashboardEvent {
  // Generic gateway events are normalized to Heartbeat until the gateway emits
  // the new DashboardEvent envelope. This keeps the client resilient.
  if ('kind' in parsed && typeof parsed.kind === 'string') {
    return parsed as DashboardEvent
  }
  return { kind: 'Heartbeat', seq: Number((parsed as { seq?: number }).seq) || 0 }
}
