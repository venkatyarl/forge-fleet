import { create } from 'zustand'
import { persist } from 'zustand/middleware'

export type Density = 'compact' | 'normal'

interface UIState {
  commandPaletteOpen: boolean
  sidebarCollapsed: boolean
  density: Density
  setCommandPaletteOpen: (open: boolean) => void
  toggleSidebar: () => void
  setDensity: (density: Density) => void
}

export const useUIStore = create<UIState>()(
  persist(
    (set) => ({
      commandPaletteOpen: false,
      sidebarCollapsed: false,
      density: 'compact',
      setCommandPaletteOpen: (open) => set({ commandPaletteOpen: open }),
      toggleSidebar: () => set((s) => ({ sidebarCollapsed: !s.sidebarCollapsed })),
      setDensity: (density) => set({ density }),
    }),
    { name: 'ff-dashboard-ui' }
  )
)
