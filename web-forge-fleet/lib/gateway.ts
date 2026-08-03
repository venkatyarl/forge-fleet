// Gateway base-URL resolution for the statically-exported Next app.
//
// - NEXT_PUBLIC_FF_GATEWAY_URL set → all traffic (HTTP, WS, SSE) goes there.
// - Development without it → HTTP calls stay same-origin ('' — Next dev
//   rewrites proxy /api, /v1, /mcp, /slm to the gateway) but WS and SSE must
//   connect DIRECTLY to the gateway because Next rewrites do not proxy
//   websockets.
// - Production (static export served by ff-gateway) → same-origin for
//   everything.

function gatewayHttpBase(): string {
  const explicit = process.env.NEXT_PUBLIC_FF_GATEWAY_URL
  if (explicit) return explicit.replace(/\/$/, '')
  return ''
}

function gatewayStreamBase(): string {
  const explicit = process.env.NEXT_PUBLIC_FF_GATEWAY_URL
  if (explicit) return explicit.replace(/\/$/, '')
  if (process.env.NODE_ENV === 'development') return 'http://127.0.0.1:8787'
  return ''
}

export function apiUrl(path: string): string {
  return `${gatewayHttpBase()}${path}`
}

export function sseUrl(path: string): string {
  return `${gatewayStreamBase()}${path}`
}

export function wsUrl(path: string): string {
  const base = gatewayStreamBase()
  if (base) {
    return `${base.replace(/^http/, 'ws')}${path}`
  }
  // SSR/prerender: no window — the socket only connects in useEffect, so a
  // placeholder is safe here.
  if (typeof window === 'undefined') return path
  // Same-origin: derive ws/wss from the current page location.
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${protocol}//${window.location.host}${path}`
}
