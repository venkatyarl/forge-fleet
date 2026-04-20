import { useEffect, useRef, useState } from 'react'

// Shape of events emitted by /api/events/stream. The backend bridges
// NATS `fleet.events.>` subjects into SSE. Each `data:` line is a JSON
// object of this shape.
export type FleetEvent = {
  subject: string
  payload: unknown
  received_at?: string
}

export type FleetEventHandler = (event: FleetEvent) => void

// Subscribe to the fleet events SSE stream. Returns a `live` flag that
// the UI can surface (green dot when true). If SSE is unavailable (503,
// network error), `live` stays false and the caller should keep their
// existing polling interval — the stream is additive.
export function useFleetEvents(handler: FleetEventHandler): { live: boolean } {
  const [live, setLive] = useState(false)
  // Stash the latest handler in a ref so we don't re-subscribe on
  // every render — subscribing once per mount is critical for SSE.
  const handlerRef = useRef(handler)
  handlerRef.current = handler

  useEffect(() => {
    let es: EventSource | null = null
    let cancelled = false
    let retryTimer: ReturnType<typeof setTimeout> | null = null

    const connect = () => {
      if (cancelled) return
      try {
        es = new EventSource('/api/events/stream')
      } catch {
        setLive(false)
        return
      }
      es.onopen = () => {
        if (!cancelled) setLive(true)
      }
      es.onmessage = (e) => {
        try {
          const parsed = JSON.parse(e.data) as FleetEvent
          handlerRef.current(parsed)
        } catch {
          // ignore malformed payloads
        }
      }
      es.onerror = () => {
        setLive(false)
        // EventSource auto-reconnects, but if the server returns 503
        // the browser will retry immediately in a tight loop — back
        // off ourselves.
        if (es) {
          es.close()
          es = null
        }
        if (!cancelled) {
          retryTimer = setTimeout(connect, 15_000)
        }
      }
    }

    connect()

    return () => {
      cancelled = true
      if (retryTimer) clearTimeout(retryTimer)
      if (es) es.close()
    }
  }, [])

  return { live }
}
