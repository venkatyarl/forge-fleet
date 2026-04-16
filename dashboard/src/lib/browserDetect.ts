/**
 * Client-side detection for onboarding pre-fill.
 * Used by OperatorOnboarding to auto-populate the form so the copy-paste
 * curl command is already tailored to the machine before the operator pastes.
 * See plan: §4 "Browser-driven pre-detection".
 */

export type OsFamily = 'mac' | 'linux' | 'windows' | 'unknown'
export type MachineKind =
  | 'apple-silicon'
  | 'intel-mac'
  | 'linux'
  | 'linux-gpu'
  | 'dgx-os'
  | 'windows'
  | 'windows-gpu'
  | 'unknown'

export interface DetectedClient {
  os_family: OsFamily
  /** True if the browser reports an Apple Silicon CPU. */
  apple_silicon: boolean
  /** Logical CPU cores as reported by the browser. Often rounded. */
  cores: number
  /** navigator.deviceMemory (GiB) — rough, capped at 8. */
  ram_gb_hint: number
  /** WebGL unmasked renderer string — can reveal GPU chip on Chrome. */
  webgl_renderer: string | null
  /** LAN IP from WebRTC ICE candidate scrape; null on privacy-strict browsers. */
  lan_ip: string | null
  timezone: string
  /** The user's best-guess machine kind based on all signals above. */
  suggested_kind: MachineKind
}

/** Parse OS family from navigator.userAgent and navigator.platform. */
function detectOsFamily(): OsFamily {
  const ua = navigator.userAgent.toLowerCase()
  const platform = (navigator.platform || '').toLowerCase()
  if (ua.includes('mac') || platform.includes('mac')) return 'mac'
  if (ua.includes('windows') || platform.includes('win')) return 'windows'
  if (ua.includes('linux') || platform.includes('linux')) return 'linux'
  return 'unknown'
}

/** Apple Silicon detection — M1/M2/M3/M4 identify as "Macintosh" + arm64. */
function detectAppleSilicon(): boolean {
  if (detectOsFamily() !== 'mac') return false
  // Chrome on Apple Silicon exposes `navigator.userAgentData.platform === "macOS"` and
  // `architecture === "arm"`. Safari hides this. Best-effort detection.
  // @ts-expect-error — userAgentData isn't on all browser types
  const uaData = navigator.userAgentData
  if (uaData && uaData.getHighEntropyValues) {
    // Can't await here; we'll do a best-effort sync check via GPU string
  }
  // Heuristic: Mac + 8+ cores + maxTouchPoints === 0 is almost always Apple Silicon.
  return navigator.hardwareConcurrency >= 8 && navigator.maxTouchPoints === 0
}

/** WebGL renderer string — may reveal GPU chip on Chrome/Edge. */
function detectWebGlRenderer(): string | null {
  try {
    const canvas = document.createElement('canvas')
    const gl = canvas.getContext('webgl') || (canvas.getContext('experimental-webgl') as WebGLRenderingContext | null)
    if (!gl) return null
    const ext = gl.getExtension('WEBGL_debug_renderer_info')
    if (!ext) return null
    const renderer = gl.getParameter(ext.UNMASKED_RENDERER_WEBGL)
    return typeof renderer === 'string' ? renderer : null
  } catch {
    return null
  }
}

/**
 * Scrape the LAN IP from a WebRTC ICE candidate. Works on most browsers but
 * is increasingly restricted for privacy; null is an acceptable fallback.
 */
function detectLanIp(timeoutMs = 800): Promise<string | null> {
  return new Promise((resolve) => {
    let resolved = false
    const finish = (value: string | null) => {
      if (resolved) return
      resolved = true
      resolve(value)
    }
    try {
      const pc = new RTCPeerConnection({ iceServers: [] })
      pc.createDataChannel('ff-onboard-detect')
      pc.onicecandidate = (evt) => {
        const c = evt.candidate?.candidate
        if (!c) return
        const parts = c.split(' ')
        for (const p of parts) {
          if (
            /^\d+\.\d+\.\d+\.\d+$/.test(p) &&
            !p.startsWith('0.') &&
            !p.startsWith('127.') &&
            !p.startsWith('169.254')
          ) {
            pc.close()
            finish(p)
            return
          }
        }
      }
      pc.createOffer()
        .then((offer) => pc.setLocalDescription(offer))
        .catch(() => finish(null))
      setTimeout(() => {
        try {
          pc.close()
        } catch {
          /* noop */
        }
        finish(null)
      }, timeoutMs)
    } catch {
      finish(null)
    }
  })
}

function suggestKind(os: OsFamily, appleSilicon: boolean, webgl: string | null): MachineKind {
  const hasNvidia = webgl ? /nvidia|geforce|quadro|tesla|rtx|dgx/i.test(webgl) : false
  if (os === 'mac') return appleSilicon ? 'apple-silicon' : 'intel-mac'
  if (os === 'linux') return hasNvidia ? 'linux-gpu' : 'linux'
  if (os === 'windows') return hasNvidia ? 'windows-gpu' : 'windows'
  return 'unknown'
}

export async function detectClient(): Promise<DetectedClient> {
  const os_family = detectOsFamily()
  const apple_silicon = detectAppleSilicon()
  const cores = navigator.hardwareConcurrency || 1
  const rawRam = (navigator as { deviceMemory?: number }).deviceMemory
  const ram_gb_hint = typeof rawRam === 'number' && rawRam > 0 ? rawRam : 0
  const webgl_renderer = detectWebGlRenderer()
  const lan_ip = await detectLanIp()
  const timezone = Intl.DateTimeFormat().resolvedOptions().timeZone
  const suggested_kind = suggestKind(os_family, apple_silicon, webgl_renderer)
  return {
    os_family,
    apple_silicon,
    cores,
    ram_gb_hint,
    webgl_renderer,
    lan_ip,
    timezone,
    suggested_kind,
  }
}
