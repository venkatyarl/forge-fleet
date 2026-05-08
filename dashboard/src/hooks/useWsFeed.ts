import { useEffect, useMemo, useRef, useState } from 'react'
import { parseWsMessage } from '../lib/normalizers'
import type { WsEvent } from '../types'

const INITIAL_RECONNECT_MS = 1000
const MAX_RECONNECT_MS = 30000
const BACKOFF_MULTIPLIER = 2

export function useWsFeed(path = '/ws') {
  const [connected, setConnected] = useState(false)
  const [lastEvent, setLastEvent] = useState<WsEvent | null>(null)
  const [eventCount, setEventCount] = useState(0)
  const reconnectTimer = useRef<number | null>(null)
  const reconnectDelay = useRef(INITIAL_RECONNECT_MS)

  const wsUrl = useMemo(() => {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    return `${protocol}//${window.location.host}${path}`
  }, [path])

  useEffect(() => {
    let socket: WebSocket | null = null
    let alive = true

    const connect = () => {
      socket = new WebSocket(wsUrl)

      socket.onopen = () => {
        setConnected(true)
        reconnectDelay.current = INITIAL_RECONNECT_MS
      }
      socket.onclose = () => {
        setConnected(false)
        if (alive) {
          reconnectTimer.current = window.setTimeout(connect, reconnectDelay.current)
          reconnectDelay.current = Math.min(
            reconnectDelay.current * BACKOFF_MULTIPLIER,
            MAX_RECONNECT_MS
          )
        }
      }
      socket.onerror = () => setConnected(false)
      socket.onmessage = (event) => {
        setEventCount((prev) => prev + 1)
        setLastEvent(parseWsMessage(String(event.data)))
      }
    }

    connect()

    return () => {
      alive = false
      if (reconnectTimer.current) {
        window.clearTimeout(reconnectTimer.current)
      }
      socket?.close()
    }
  }, [wsUrl])

  return { connected, lastEvent, eventCount }
}
