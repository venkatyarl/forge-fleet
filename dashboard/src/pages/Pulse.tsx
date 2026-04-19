import { useState } from 'react'
import { FleetOverviewPanel } from '../components/FleetOverviewPanel'
import { LeaderPanel } from '../components/LeaderPanel'
import { LlmTopologyPanel } from '../components/LlmTopologyPanel'
import { SoftwareDriftPanel } from '../components/SoftwareDriftPanel'
import { ProjectsPanel } from '../components/ProjectsPanel'
import { DbHaPanel } from '../components/DbHaPanel'
import { AlertsPanel } from '../components/AlertsPanel'
import { DockerPanel } from '../components/DockerPanel'

type Tab =
  | 'overview'
  | 'leader'
  | 'llms'
  | 'software'
  | 'projects'
  | 'ha'
  | 'alerts'
  | 'docker'

const TABS: Array<{ id: Tab; label: string; icon: string }> = [
  { id: 'overview', label: 'Overview', icon: '🖥️' },
  { id: 'leader', label: 'Leader', icon: '👑' },
  { id: 'llms', label: 'LLMs', icon: '🤖' },
  { id: 'software', label: 'Software', icon: '📦' },
  { id: 'projects', label: 'Projects', icon: '📁' },
  { id: 'ha', label: 'HA', icon: '🗄️' },
  { id: 'alerts', label: 'Alerts', icon: '🚨' },
  { id: 'docker', label: 'Docker', icon: '🐳' },
]

export function Pulse() {
  const [tab, setTab] = useState<Tab>(() => {
    const hash = window.location.hash.replace(/^#/, '')
    return (TABS.find((t) => t.id === hash)?.id ?? 'overview') as Tab
  })

  const selectTab = (t: Tab) => {
    setTab(t)
    history.replaceState(null, '', `#${t}`)
  }

  return (
    <div className="space-y-4">
      <nav className="flex flex-wrap gap-1 border-b border-zinc-800 pb-2">
        {TABS.map((t) => {
          const active = t.id === tab
          return (
            <button
              key={t.id}
              onClick={() => selectTab(t.id)}
              className={`flex items-center gap-1.5 rounded-md px-3 py-1.5 text-sm transition ${
                active
                  ? 'bg-violet-500/15 text-violet-300'
                  : 'text-zinc-400 hover:bg-zinc-800/70 hover:text-zinc-200'
              }`}
            >
              <span>{t.icon}</span>
              <span>{t.label}</span>
            </button>
          )
        })}
      </nav>

      <div>
        {tab === 'overview' && <FleetOverviewPanel />}
        {tab === 'leader' && <LeaderPanel />}
        {tab === 'llms' && <LlmTopologyPanel />}
        {tab === 'software' && <SoftwareDriftPanel />}
        {tab === 'projects' && <ProjectsPanel />}
        {tab === 'ha' && <DbHaPanel />}
        {tab === 'alerts' && <AlertsPanel />}
        {tab === 'docker' && <DockerPanel />}
      </div>
    </div>
  )
}
