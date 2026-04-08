import { useEffect, useRef } from 'react'
import { useNavigate } from 'react-router-dom'

/**
 * Global keyboard shortcuts for ForgeFleet dashboard.
 * Uses two-key chord pattern: press G then H for Home, G then T for Topology, etc.
 */
export function useKeyboardShortcuts() {
  const navigate = useNavigate()
  const pendingKey = useRef<string | null>(null)
  const timeout = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      // Don't trigger in input fields
      const target = e.target as HTMLElement
      if (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable) return

      const key = e.key.toLowerCase()

      // ? shows help
      if (key === '?' && !e.metaKey && !e.ctrlKey) {
        e.preventDefault()
        alert('Keyboard Shortcuts:\n\nG then H — Home\nG then T — Topology\nG then M — Mission Control\nG then C — Chat Studio\nG then P — Projects\nG then S — Settings\nG then K — Tools\n\n⌘K — Command Palette\n? — This help')
        return
      }

      // Two-key chords starting with G
      if (pendingKey.current === 'g') {
        e.preventDefault()
        pendingKey.current = null
        if (timeout.current) clearTimeout(timeout.current)

        switch (key) {
          case 'h': navigate('/'); break
          case 't': navigate('/topology'); break
          case 'm': navigate('/mission-control'); break
          case 'c': navigate('/chat'); break
          case 'p': navigate('/projects'); break
          case 's': navigate('/settings'); break
          case 'k': navigate('/tools'); break
          case 'a': navigate('/audit'); break
          case 'n': navigate('/model-hub'); break
          case 'w': navigate('/workflow'); break
          case 'l': navigate('/planning'); break
        }
        return
      }

      // Start chord with G
      if (key === 'g' && !e.metaKey && !e.ctrlKey) {
        pendingKey.current = 'g'
        if (timeout.current) clearTimeout(timeout.current)
        timeout.current = setTimeout(() => { pendingKey.current = null }, 1000)
        return
      }
    }

    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [navigate])
}
