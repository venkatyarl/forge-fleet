import { Suspense, useEffect, useState } from 'react'
import { Navigate, Outlet, Route, Routes, useLocation } from 'react-router-dom'
import { useQueryClient } from '@tanstack/react-query'
import { Header } from './components/Header'
import { Sidebar } from './components/Sidebar'
import { CommandPalette } from './components/CommandPalette'
import { ErrorBoundary } from './components/ErrorBoundary'
import { PageSkeleton } from './components/PageSkeleton'
import { useDashboardWebSocket } from './sync/ws-client'
import { reduceDashboardEvent } from './sync/events'
import { useUIStore } from './app/store'
import { useKeyboardShortcuts } from './hooks/useKeyboardShortcuts'
import { lazyNamed } from './lib/lazy'

// ---------------------------------------------------------------------------
// Lazy-loaded pages
// ---------------------------------------------------------------------------

const MissionControl = lazyNamed(() => import('./pages/MissionControl'), 'MissionControl')
const MyTasks = lazyNamed(() => import('./pages/MyTasks'), 'MyTasks')
const BuildPipeline = lazyNamed(() => import('./pages/BuildPipeline'), 'BuildPipeline')
const Projects = lazyNamed(() => import('./pages/Projects'), 'Projects')
const PlanningHub = lazyNamed(() => import('./pages/PlanningHub'), 'PlanningHub')
const WorkflowWorkbench = lazyNamed(() => import('./pages/WorkflowWorkbench'), 'WorkflowWorkbench')
const Brain = lazyNamed(() => import('./pages/Brain'), 'Brain')
const BrainGraph = lazyNamed(() => import('./pages/BrainGraph'), 'BrainGraph')
const Pulse = lazyNamed(() => import('./pages/Pulse'), 'Pulse')
const FleetOverview = lazyNamed(() => import('./pages/FleetOverview'), 'FleetOverview')
const Topology = lazyNamed(() => import('./pages/Topology'), 'Topology')
const MeshStatus = lazyNamed(() => import('./pages/MeshStatus'), 'MeshStatus')
const ModelHub = lazyNamed(() => import('./pages/ModelHub'), 'ModelHub')
const ModelInventory = lazyNamed(() => import('./pages/ModelInventory'), 'ModelInventory')
const SlmStatus = lazyNamed(() => import('./pages/slm-status'), 'SlmStatus')
const ToolInventory = lazyNamed(() => import('./pages/ToolInventory'), 'ToolInventory')
const Metrics = lazyNamed(() => import('./pages/Metrics'), 'Metrics')
const Alerts = lazyNamed(() => import('./pages/Alerts'), 'Alerts')
const Settings = lazyNamed(() => import('./pages/Settings'), 'Settings')
const ConfigEditor = lazyNamed(() => import('./pages/ConfigEditor'), 'ConfigEditor')
const LLMProxy = lazyNamed(() => import('./pages/LLMProxy'), 'LLMProxy')
const AuditLog = lazyNamed(() => import('./pages/AuditLog'), 'AuditLog')
const Updates = lazyNamed(() => import('./pages/Updates'), 'Updates')
const OperatorOnboarding = lazyNamed(() => import('./pages/OperatorOnboarding'), 'OperatorOnboarding')
const Versions = lazyNamed(() => import('./pages/Versions'), 'Versions')
const CostLedger = lazyNamed(() => import('./pages/CostLedger'), 'CostLedger')
const NodeDetail = lazyNamed(() => import('./pages/NodeDetail'), 'NodeDetail')
const NotFound = lazyNamed(() => import('./pages/NotFound'), 'NotFound')
const Agents = lazyNamed(() => import('./pages/Agents'), 'Agents')
const Council = lazyNamed(() => import('./pages/Council'), 'Council')
const Mcp = lazyNamed(() => import('./pages/Mcp'), 'Mcp')
const Skills = lazyNamed(() => import('./pages/Skills'), 'Skills')
const Interactions = lazyNamed(() => import('./pages/Interactions'), 'Interactions')

function page(Component: React.ComponentType) {
  return (
    <Suspense fallback={<PageSkeleton />}>
      <Component />
    </Suspense>
  )
}

function Shell() {
  useKeyboardShortcuts()
  useRouteTitle()
  const queryClient = useQueryClient()
  const [darkMode, setDarkMode] = useState(() => {
    const cached = localStorage.getItem('ff_dark_mode')
    return cached ? cached === 'true' : true
  })
  const sidebarCollapsed = useUIStore((s) => s.sidebarCollapsed)
  const { connected, eventCount, lastEvent } = useDashboardWebSocket('/ws', (event) => {
    reduceDashboardEvent(queryClient, event)
  })

  useEffect(() => {
    document.documentElement.classList.toggle('dark', darkMode)
    localStorage.setItem('ff_dark_mode', String(darkMode))
  }, [darkMode])

  return (
    <div className="min-h-screen bg-background text-foreground">
      <CommandPalette />
      <Header
        wsConnected={connected}
        eventCount={eventCount}
        lastEvent={lastEvent}
        darkMode={darkMode}
        onToggleDarkMode={() => setDarkMode((prev) => !prev)}
      />

      <div className="flex h-[calc(100vh-49px)] flex-col md:flex-row">
        <Sidebar collapsed={sidebarCollapsed} />
        <main className="flex-1 overflow-y-auto bg-surface p-4 md:p-6">
          <ErrorBoundary>
            <Outlet context={{ wsEvent: lastEvent }} />
          </ErrorBoundary>
        </main>
      </div>
    </div>
  )
}

// Browser-tab titles per route. Derived from useLocation()'s pathname rather
// than useMatches(): useMatches() ONLY works under a DATA router
// (createBrowserRouter), but this app uses the declarative
// <BrowserRouter>/<Routes> component router — calling useMatches() there throws
// "useMatches must be used within a data router", and because Shell renders
// OUTSIDE the per-page ErrorBoundary that threw blanked the whole dashboard.
const ROUTE_TITLES: Record<string, string> = {
  '/': 'Mission Control',
  '/my-tasks': 'My Tasks',
  '/build-pipeline': 'Build Pipeline',
  '/projects': 'Projects',
  '/planning': 'Planning Hub',
  '/workflow': 'Workflows',
  '/brain': 'Brain',
  '/brain/graph': 'Knowledge Graph',
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
}

function useRouteTitle() {
  const { pathname } = useLocation()
  useEffect(() => {
    // Exact match first; otherwise longest matching prefix (covers param
    // sub-routes like /brain/:threadSlug → "Brain").
    const title =
      ROUTE_TITLES[pathname] ??
      Object.entries(ROUTE_TITLES)
        .filter(([p]) => p !== '/' && pathname.startsWith(p + '/'))
        .sort((a, b) => b[0].length - a[0].length)[0]?.[1]
    document.title = title ? `${title} · ForgeFleet` : 'ForgeFleet'
  }, [pathname])
}

export default function App() {
  return (
    <Routes>
      <Route path="/" element={<Shell />}>
        {/* Mission Control = home */}
        <Route index element={page(MissionControl)} handle={{ title: 'Mission Control' }} />
        {/* Chats — consolidated into Brain */}
        <Route path="chat" element={<Navigate to="/brain" replace />} />
        <Route path="chat/:chatId" element={<Navigate to="/brain" replace />} />
        <Route path="chats" element={<Navigate to="/brain" replace />} />
        {/* Project Management */}
        <Route path="my-tasks" element={page(MyTasks)} handle={{ title: 'My Tasks' }} />
        <Route path="build-pipeline" element={page(BuildPipeline)} handle={{ title: 'Build Pipeline' }} />
        <Route path="projects" element={page(Projects)} handle={{ title: 'Projects' }} />
        <Route path="planning" element={page(PlanningHub)} handle={{ title: 'Planning Hub' }} />
        <Route path="workflow" element={page(WorkflowWorkbench)} handle={{ title: 'Workflows' }} />
        {/* Brain */}
        <Route path="brain" element={page(Brain)} handle={{ title: 'Brain' }} />
        <Route path="brain/graph" element={page(BrainGraph)} handle={{ title: 'Knowledge Graph' }} />
        <Route path="brain/:threadSlug" element={page(Brain)} handle={{ title: 'Brain Thread' }} />
        {/* Intelligence / agents */}
        <Route path="agents" element={page(Agents)} handle={{ title: 'Agents & Swarm' }} />
        <Route path="council" element={page(Council)} handle={{ title: 'Council' }} />
        <Route path="mcp" element={page(Mcp)} handle={{ title: 'MCP' }} />
        <Route path="skills" element={page(Skills)} handle={{ title: 'Skills' }} />
        <Route path="interactions" element={page(Interactions)} handle={{ title: 'Interactions' }} />
        {/* Pulse v2 dashboard */}
        <Route path="pulse" element={page(Pulse)} handle={{ title: 'Pulse' }} />
        {/* Fleet */}
        <Route path="fleet" element={page(FleetOverview)} handle={{ title: 'Fleet Overview' }} />
        <Route path="topology" element={page(Topology)} handle={{ title: 'Topology' }} />
        <Route path="model-hub" element={page(ModelHub)} handle={{ title: 'Model Hub' }} />
        <Route path="models" element={page(ModelInventory)} handle={{ title: 'Model Inventory' }} />
        <Route path="slm-status" element={page(SlmStatus)} handle={{ title: 'SLM Status' }} />
        <Route path="tools" element={page(ToolInventory)} handle={{ title: 'Tool Inventory' }} />
        <Route path="metrics" element={page(Metrics)} handle={{ title: 'Metrics' }} />
        <Route path="alerts" element={page(Alerts)} handle={{ title: 'Alerts' }} />
        {/* Settings */}
        <Route path="settings" element={page(Settings)} handle={{ title: 'Settings' }} />
        <Route path="config" element={page(ConfigEditor)} handle={{ title: 'Config Editor' }} />
        <Route path="llm-proxy" element={page(LLMProxy)} handle={{ title: 'LLM Proxy' }} />
        <Route path="audit" element={page(AuditLog)} handle={{ title: 'Audit Log' }} />
        <Route path="updates" element={page(Updates)} handle={{ title: 'Updates' }} />
        <Route path="onboarding" element={page(OperatorOnboarding)} handle={{ title: 'Onboarding' }} />
        <Route path="onboard" element={<Navigate to="/onboarding" replace />} />
        <Route path="versions" element={page(Versions)} handle={{ title: 'Versions' }} />
        <Route path="mesh" element={page(MeshStatus)} handle={{ title: 'Mesh Status' }} />
        <Route path="cost-ledger" element={page(CostLedger)} handle={{ title: 'Cost Ledger' }} />
        {/* Legacy redirects */}
        <Route path="mission-control" element={<Navigate to="/" replace />} />
        <Route path="nodes/:nodeId" element={page(NodeDetail)} handle={{ title: 'Node Detail' }} />
        <Route path="*" element={page(NotFound)} handle={{ title: 'Not Found' }} />
      </Route>
    </Routes>
  )
}
