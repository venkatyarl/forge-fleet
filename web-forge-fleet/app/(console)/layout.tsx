'use client'

import { createContext, useContext, useEffect } from 'react'
import { usePathname } from 'next/navigation'
import { useQueryClient } from '@tanstack/react-query'
import { Header } from '@/components/Header'
import { Sidebar } from '@/components/Sidebar'
import { CommandPalette } from '@/components/CommandPalette'
import { ErrorBoundary } from '@/components/ErrorBoundary'
import { useDashboardWebSocket } from '@/sync/ws-client'
import { reduceDashboardEvent } from '@/sync/events'
import { useUIStore } from '@/app/store'
import { useDarkMode } from '@/app/providers'
import type { DashboardEvent } from '@/protocol/types'
import type { WsEvent } from '@/types'

// Replacement for the dashboard's <Outlet context={{ wsEvent: lastEvent }} />.
// Pages that used useOutletContext() (FleetOverview, AuditLog, LLMProxy) read
// this via useWsEvent() instead. The runtime value is a DashboardEvent from
// /ws, exactly as the Outlet context carried; it is typed as WsEvent to match
// what those pages consume.
export const WsEventContext = createContext<WsEvent | null>(null)

export function useWsEvent(): WsEvent | null {
  return useContext(WsEventContext)
}

// Browser-tab titles per route, ported from the dashboard's App.tsx. Uses
// usePathname() instead of react-router's useLocation().
const ROUTE_TITLES: Record<string, string> = {
  '/': 'Mission Control',
  '/my-tasks': 'My Tasks',
  '/build-pipeline': 'Build Pipeline',
  '/projects': 'Projects',
  '/planning': 'Planning Hub',
  '/workflow': 'Workflows',
  '/brain': 'Brain',
  '/brain/graph': 'Knowledge Graph',
  '/cortex': 'Cortex',
  '/agents': 'Agents & Swarm',
  '/council': 'Council',
  '/mcp': 'MCP',
  '/skills': 'Skills',
  '/interactions': 'Interactions',
  '/pulse': 'Pulse',
  '/fleet': 'Fleet Overview',
  '/topology': 'Topology',
  '/model-hub': 'Model Hub',
  '/models': 'Model Inventory',
  '/slm-status': 'SLM Status',
  '/tools': 'Tool Inventory',
  '/metrics': 'Metrics',
  '/alerts': 'Alerts',
  '/settings': 'Settings',
  '/config': 'Config Editor',
  '/llm-proxy': 'LLM Proxy',
  '/audit': 'Audit Log',
  '/updates': 'Updates',
  '/onboarding': 'Onboarding',
  '/versions': 'Versions',
  '/mesh': 'Mesh Status',
  '/cost-ledger': 'Cost Ledger',
  '/training': 'Training',
  '/queue': 'Deferred Queue',
  '/jira': 'Jira Monitor',
}

function useRouteTitle() {
  const pathname = usePathname()
  useEffect(() => {
    // Exact match first; otherwise longest matching prefix (covers param
    // sub-routes like /brain/[threadSlug] → "Brain").
    const title =
      ROUTE_TITLES[pathname] ??
      Object.entries(ROUTE_TITLES)
        .filter(([p]) => p !== '/' && pathname.startsWith(p + '/'))
        .sort((a, b) => b[0].length - a[0].length)[0]?.[1]
    document.title = title ? `${title} · ForgeFleet` : 'ForgeFleet'
  }, [pathname])
}

export default function ConsoleLayout({ children }: { children: React.ReactNode }) {
  useRouteTitle()
  const queryClient = useQueryClient()
  const { darkMode, toggleDarkMode } = useDarkMode()
  const sidebarCollapsed = useUIStore((s) => s.sidebarCollapsed)
  const { connected, eventCount, lastEvent } = useDashboardWebSocket('/ws', (event) => {
    reduceDashboardEvent(queryClient, event)
  })

  return (
    <div className="min-h-screen bg-background text-foreground">
      <CommandPalette />
      <Header
        wsConnected={connected}
        eventCount={eventCount}
        lastEvent={lastEvent}
        darkMode={darkMode}
        onToggleDarkMode={toggleDarkMode}
      />

      <div className="flex h-[calc(100vh-49px)] flex-col md:flex-row">
        <Sidebar collapsed={sidebarCollapsed} />
        <main className="flex-1 overflow-y-auto bg-surface p-4 md:p-6">
          <ErrorBoundary>
            <WsEventContext.Provider value={lastEvent as unknown as WsEvent | null}>
              {children}
            </WsEventContext.Provider>
          </ErrorBoundary>
        </main>
      </div>
    </div>
  )
}

export type { DashboardEvent }
