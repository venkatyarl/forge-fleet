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
import { NodeDetail } from './pages/NodeDetail'
import { OperatorOnboarding } from './pages/OperatorOnboarding'
import { PlanningHub } from './pages/PlanningHub'
import { Projects } from './pages/Projects'
import { Topology } from './pages/Topology'
import { Updates } from './pages/Updates'
import { WorkflowWorkbench } from './pages/WorkflowWorkbench'
import { Settings } from './pages/Settings'
import { Chats } from './pages/Chats'
import { ToolInventory } from './pages/ToolInventory'
import { ModelHub } from './pages/ModelHub'

function Shell() {
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
    <div className="min-h-screen bg-slate-950 text-slate-100">
      <CommandPalette />
      <Header
        wsConnected={connected}
        eventCount={eventCount}
        lastEvent={lastEvent}
        darkMode={darkMode}
        onToggleDarkMode={() => setDarkMode((prev) => !prev)}
      />

      <div className="flex min-h-[calc(100vh-85px)] flex-col md:flex-row">
        <Sidebar />
        <main className="flex-1 p-4 md:p-6">
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
        <Route index element={<FleetOverview />} />
        <Route path="nodes/:nodeId" element={<NodeDetail />} />
        <Route path="models" element={<ModelInventory />} />
        <Route path="model-hub" element={<ModelHub />} />
        <Route path="tools" element={<ToolInventory />} />
        <Route path="settings" element={<Settings />} />
        <Route path="config" element={<ConfigEditor />} />
        <Route path="mission-control" element={<MissionControl />} />
        <Route path="onboarding" element={<OperatorOnboarding />} />
        <Route path="my-tasks" element={<MyTasks />} />
        <Route path="workflow" element={<WorkflowWorkbench />} />
        <Route path="planning" element={<PlanningHub />} />
        <Route path="projects" element={<Projects />} />
        <Route path="chats" element={<Chats />} />
        <Route path="chat" element={<ChatStudio />} />
        <Route path="chat/:chatId" element={<ChatStudio />} />
        <Route path="llm-proxy" element={<LLMProxy />} />
        <Route path="topology" element={<Topology />} />
        <Route path="audit" element={<AuditLog />} />
        <Route path="updates" element={<Updates />} />
        <Route path="metrics" element={<Metrics />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  )
}
