import { useEffect, useMemo, useRef, useState } from 'react'
import { parseWsMessage } from '../lib/normalizers'
import type { WsEvent } from '../types'

export function useWsFeed(path = '/ws') {
  const [connected, setConnected] = useState(false)
  const [lastEvent, setLastEvent] = useState<WsEvent | null>(null)
  const [eventCount, setEventCount] = useState(0)
  const reconnectTimer = useRef<number | null>(null)

  const wsUrl = useMemo(() => {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    return `${protocol}//${window.location.host}${path}`
  }, [path])

  useEffect(() => {
    let socket: WebSocket | null = null
    let alive = true

    const connect = () => {
      socket = new WebSocket(wsUrl)

      socket.onopen = () => setConnected(true)
      socket.onclose = () => {
        setConnected(false)
        if (alive) {
          reconnectTimer.current = window.setTimeout(connect, 3000)
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
