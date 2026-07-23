import { useState, type ElementType } from 'react'
import {
  Bell,
  Bot,
  Box,
  Crown,
  Database,
  Folder,
  Monitor,
  Package,
} from 'lucide-react'
import { FleetOverviewPanel } from '../components/FleetOverviewPanel'
import { LeaderPanel } from '../components/LeaderPanel'
import { LlmTopologyPanel } from '../components/LlmTopologyPanel'
import { SoftwareDriftPanel } from '../components/SoftwareDriftPanel'
import { ProjectsPanel } from '../components/ProjectsPanel'
import { DbHaPanel } from '../components/DbHaPanel'
import { AlertsPanel } from '../components/AlertsPanel'
import { DockerPanel } from '../components/DockerPanel'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { Badge } from '../components/ui/badge'
import { StatusBadge } from '../components/ui/status-badge'
import { Button } from '../components/ui/button'
import { cn } from '../lib/utils'

type Tab =
  | 'overview'
  | 'leader'
  | 'llms'
  | 'software'
  | 'projects'
  | 'ha'
  | 'alerts'
  | 'docker'

const TABS: Array<{ id: Tab; label: string; description: string; icon: ElementType }> = [
  { id: 'overview', label: 'Overview', description: 'Fleet health and capacity', icon: Monitor },
  { id: 'leader', label: 'Leader', description: 'Consensus and authority', icon: Crown },
  { id: 'llms', label: 'LLMs', description: 'Model topology', icon: Bot },
  { id: 'software', label: 'Software', description: 'Version drift', icon: Package },
  { id: 'projects', label: 'Projects', description: 'Active project map', icon: Folder },
  { id: 'ha', label: 'HA', description: 'Database availability', icon: Database },
  { id: 'alerts', label: 'Alerts', description: 'Open incidents', icon: Bell },
  { id: 'docker', label: 'Docker', description: 'Container state', icon: Box },
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

  const activeTab = TABS.find((t) => t.id === tab) ?? TABS[0]

  return (
    <section className="min-h-full space-y-5 bg-background text-foreground">
      <Card className="bg-surface">
        <CardHeader className="mb-4 flex-col items-start gap-3 sm:flex-row sm:items-center">
          <div>
            <div className="flex flex-wrap items-center gap-2">
              <CardTitle className="text-base">Fleet Pulse</CardTitle>
              <StatusBadge status="active">live</StatusBadge>
              <Badge variant="neutral">{TABS.length} views</Badge>
            </div>
            <CardDescription className="mt-1">
              {activeTab.description}
            </CardDescription>
          </div>
          <Badge variant="default">{activeTab.label}</Badge>
        </CardHeader>

        <nav className="grid gap-2 sm:grid-cols-2 lg:grid-cols-4" aria-label="Pulse sections">
          {TABS.map((t) => {
            const active = t.id === tab
            const Icon = t.icon
            return (
              <Button
                key={t.id}
                type="button"
                variant="ghost"
                onClick={() => selectTab(t.id)}
                aria-current={active ? 'page' : undefined}
                className={cn(
                  'h-auto justify-start rounded-lg border border-border bg-panel px-3 py-2 text-left text-muted hover:border-border-subtle hover:bg-elevated hover:text-foreground',
                  active &&
                    'border-primary/40 bg-primary-subtle text-primary shadow-glow hover:bg-primary-subtle hover:text-primary'
                )}
              >
                <Icon className="h-4 w-4 shrink-0" />
                <span className="min-w-0">
                  <span className="block truncate text-sm font-medium">{t.label}</span>
                  <span className="block truncate text-2xs font-normal text-dim">
                    {t.description}
                  </span>
                </span>
              </Button>
            )
          })}
        </nav>
      </Card>

      <div className="min-w-0">
        {tab === 'overview' && <FleetOverviewPanel />}
        {tab === 'leader' && <LeaderPanel />}
        {tab === 'llms' && <LlmTopologyPanel />}
        {tab === 'software' && <SoftwareDriftPanel />}
        {tab === 'projects' && <ProjectsPanel />}
        {tab === 'ha' && <DbHaPanel />}
        {tab === 'alerts' && <AlertsPanel />}
        {tab === 'docker' && <DockerPanel />}
      </div>
    </section>
  )
}
