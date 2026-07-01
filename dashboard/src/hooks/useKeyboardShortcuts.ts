import { useEffect, useRef } from 'react'
import { useNavigate } from 'react-router-dom'
import { useUIStore } from '../app/store'
import { COMMANDS } from '../app/commands'

export function useKeyboardShortcuts() {
  const navigate = useNavigate()
  const setPaletteOpen = useUIStore((s) => s.setCommandPaletteOpen)
  const pendingKey = useRef<string | null>(null)
  const timeout = useRef<ReturnType<typeof setTimeout> | null>(null)

  const chordRoutes = COMMANDS.reduce<Record<string, string>>((acc, cmd) => {
    if (cmd.shortcut) {
      const keys = cmd.shortcut.toLowerCase().split(' ')
      if (keys.length === 2 && keys[0] === 'g' && cmd.path) {
        acc[keys[1]] = cmd.path
      }
    }
    return acc
  }, {})

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement
      if (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable) return

      const key = e.key.toLowerCase()

      if (key === '?' && !e.metaKey && !e.ctrlKey) {
        e.preventDefault()
        setPaletteOpen(true)
        return
      }

      if (pendingKey.current === 'g') {
        e.preventDefault()
        pendingKey.current = null
        if (timeout.current) clearTimeout(timeout.current)
        const path = chordRoutes[key]
        if (path) navigate(path)
        return
      }

      if (key === 'g' && !e.metaKey && !e.ctrlKey) {
        pendingKey.current = 'g'
        if (timeout.current) clearTimeout(timeout.current)
        timeout.current = setTimeout(() => {
          pendingKey.current = null
        }, 1000)
      }
    }

    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [navigate, setPaletteOpen, chordRoutes])
}
