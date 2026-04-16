import { useEffect, useState } from 'react'
import { Navigate, Outlet, Route, Routes } from 'react-router-dom'
import { Header } from './components/Header'
import { Sidebar } from './components/Sidebar'
import { CommandPalette } from './components/CommandPalette'
import { useWsFeed } from './hooks/useWsFeed'
import { AuditLog } from './pages/AuditLog'
import { ConfigEditor } from './pages/ConfigEditor'
import { ChatStudio } from './pages/ChatStudio'
import { FleetOverview } from './pages/FleetOverview'
import { LLMProxy } from './pages/LLMProxy'
import { Metrics } from './pages/Metrics'
import { MissionControl } from './pages/MissionControl'
import { ModelInventory } from './pages/ModelInventory'
import { MyTasks } from './pages/MyTasks'
import { OperatorOnboarding } from './pages/OperatorOnboarding'
import { Versions } from './pages/Versions'
import { PlanningHub } from './pages/PlanningHub'
import { Projects } from './pages/Projects'
import { Topology } from './pages/Topology'
import { Updates } from './pages/Updates'
import { WorkflowWorkbench } from './pages/WorkflowWorkbench'
import { Settings } from './pages/Settings'
import { Chats } from './pages/Chats'
import { ToolInventory } from './pages/ToolInventory'
import { ModelHub } from './pages/ModelHub'
import { useKeyboardShortcuts } from './hooks/useKeyboardShortcuts'

function Shell() {
  useKeyboardShortcuts()
  const [darkMode, setDarkMode] = useState(() => {
    const cached = localStorage.getItem('ff_dark_mode')
    return cached ? cached === 'true' : true
  })
  const { connected, eventCount, lastEvent } = useWsFeed('/ws')

  useEffect(() => {
    document.documentElement.classList.toggle('dark', darkMode)
    localStorage.setItem('ff_dark_mode', String(darkMode))
  }, [darkMode])

  return (
    <div className="min-h-screen bg-[#09090B] text-zinc-100">
      <CommandPalette />
      <Header
        wsConnected={connected}
        eventCount={eventCount}
        lastEvent={lastEvent}
        darkMode={darkMode}
        onToggleDarkMode={() => setDarkMode((prev) => !prev)}
      />

      <div className="flex h-[calc(100vh-49px)] flex-col md:flex-row">
        <Sidebar />
        <main className="flex-1 overflow-y-auto p-4 md:p-6">
          <Outlet context={{ wsEvent: lastEvent }} />
        </main>
      </div>
    </div>
  )
}

export default function App() {
  return (
    <Routes>
      <Route path="/" element={<Shell />}>
        {/* Mission Control = home */}
        <Route index element={<MissionControl />} />
        {/* Chats */}
        <Route path="chat" element={<ChatStudio />} />
        <Route path="chat/:chatId" element={<ChatStudio />} />
        <Route path="chats" element={<Chats />} />
        {/* Project Management */}
        <Route path="my-tasks" element={<MyTasks />} />
        <Route path="projects" element={<Projects />} />
        <Route path="planning" element={<PlanningHub />} />
        <Route path="workflow" element={<WorkflowWorkbench />} />
        {/* Fleet (accessible via Settings or direct link) */}
        <Route path="fleet" element={<FleetOverview />} />
        <Route path="topology" element={<Topology />} />
        <Route path="model-hub" element={<ModelHub />} />
        <Route path="models" element={<ModelInventory />} />
        <Route path="tools" element={<ToolInventory />} />
        <Route path="metrics" element={<Metrics />} />
        {/* Settings (unified page) */}
        <Route path="settings" element={<Settings />} />
        <Route path="config" element={<ConfigEditor />} />
        <Route path="llm-proxy" element={<LLMProxy />} />
        <Route path="audit" element={<AuditLog />} />
        <Route path="updates" element={<Updates />} />
        <Route path="onboarding" element={<OperatorOnboarding />} />
        <Route path="onboard" element={<Navigate to="/onboarding" replace />} />
        <Route path="versions" element={<Versions />} />
        {/* Legacy redirects */}
        <Route path="mission-control" element={<Navigate to="/" replace />} />
        <Route path="nodes/:nodeId" element={<Navigate to="/settings#fleet" replace />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  )
}
