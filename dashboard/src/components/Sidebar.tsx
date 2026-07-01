import { useState, useEffect } from 'react'
import { NavLink, useLocation } from 'react-router-dom'
import { useUIStore } from '../app/store'

type NavItem = { to: string; label: string; icon: string }
type NavSection = {
  id: string
  title: string
  icon: string
  items?: NavItem[]
  link?: string
}

const mainSections: NavSection[] = [
  { id: 'mission-control', title: 'Mission Control', icon: '🎯', link: '/' },
  { id: 'pulse', title: 'Pulse', icon: '📡', link: '/pulse' },
  {
    id: 'brain',
    title: 'Brain',
    icon: '🧠',
    items: [
      { to: '/brain', label: 'Threads', icon: '💭' },
      { to: '/brain/graph', label: 'Knowledge Graph', icon: '🕸️' },
    ],
  },
  {
    id: 'intelligence',
    title: 'Intelligence',
    icon: '🔮',
    items: [
      { to: '/agents', label: 'Agents / Swarm', icon: '🐝' },
      { to: '/council', label: 'Council', icon: '🏛️' },
      { to: '/mcp', label: 'MCP', icon: '🔌' },
      { to: '/skills', label: 'Skills', icon: '📖' },
      { to: '/interactions', label: 'Interactions', icon: '📡' },
    ],
  },
  {
    id: 'project-management',
    title: 'Project Management',
    icon: '📋',
    items: [
      { to: '/my-tasks', label: 'My Tasks', icon: '✅' },
      { to: '/projects', label: 'Projects', icon: '📁' },
      { to: '/planning', label: 'Planning Hub', icon: '🗓️' },
      { to: '/workflow', label: 'Workflows', icon: '🔄' },
    ],
  },
  { id: 'cost-ledger', title: 'Cost Ledger', icon: '💰', link: '/cost-ledger' },
  {
    id: 'fleet',
    title: 'Fleet',
    icon: '🚀',
    items: [
      { to: '/fleet', label: 'Overview', icon: '📊' },
      { to: '/topology', label: 'Topology', icon: '🕸️' },
      { to: '/mesh', label: 'Mesh Status', icon: '🔗' },
    ],
  },
  {
    id: 'models',
    title: 'Models & Tools',
    icon: '🤖',
    items: [
      { to: '/model-hub', label: 'Model Hub', icon: '🏗️' },
      { to: '/models', label: 'Inventory', icon: '📋' },
      { to: '/tools', label: 'Tool Inventory', icon: '🛠️' },
      { to: '/llm-proxy', label: 'LLM Proxy', icon: '🔀' },
    ],
  },
  {
    id: 'operations',
    title: 'Operations',
    icon: '⚙️',
    items: [
      { to: '/metrics', label: 'Metrics', icon: '📈' },
      { to: '/alerts', label: 'Alerts', icon: '🔔' },
      { to: '/audit', label: 'Audit Log', icon: '📜' },
      { to: '/updates', label: 'Updates', icon: '🔄' },
      { to: '/versions', label: 'Versions', icon: '🏷️' },
    ],
  },
  {
    id: 'system',
    title: 'System',
    icon: '🔧',
    items: [
      { to: '/settings', label: 'Settings', icon: '⚙️' },
      { to: '/config', label: 'Config Editor', icon: '📝' },
      { to: '/onboarding', label: 'Onboarding', icon: '🎓' },
    ],
  },
]

function loadExpandedState(): Record<string, boolean> {
  try {
    const raw = localStorage.getItem('ff_sidebar_expanded')
    if (raw) return JSON.parse(raw)
  } catch { /* ignore */ }
  return Object.fromEntries(mainSections.filter((s) => (s.items?.length ?? 0) > 0).map((s) => [s.id, true]))
}

interface SidebarProps {
  collapsed?: boolean
}

export function Sidebar({ collapsed: collapsedProp }: SidebarProps) {
  const storeCollapsed = useUIStore((s) => s.sidebarCollapsed)
  const toggleSidebar = useUIStore((s) => s.toggleSidebar)
  const collapsed = collapsedProp ?? storeCollapsed
  const [expanded, setExpanded] = useState<Record<string, boolean>>(loadExpandedState)
  const location = useLocation()

  useEffect(() => {
    localStorage.setItem('ff_sidebar_expanded', JSON.stringify(expanded))
  }, [expanded])

  const toggleSection = (id: string) => {
    setExpanded((prev) => ({ ...prev, [id]: !prev[id] }))
  }

  const isSectionActive = (section: NavSection) => {
    if (section.link) return location.pathname === section.link
    return (section.items ?? []).some(
      (item) => location.pathname === item.to || location.pathname.startsWith(item.to + '/')
    )
  }

  return (
    <aside
      className={`flex h-full flex-shrink-0 flex-col overflow-y-auto border-b border-border bg-elevated/80 transition-all duration-200 md:border-b-0 md:border-r ${
        collapsed ? 'md:w-14' : 'w-full md:w-56'
      } p-2`}
    >
      <button
        onClick={toggleSidebar}
        className="mb-2 hidden w-full rounded p-1 text-xs text-dim hover:bg-panel hover:text-muted md:block"
        title={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
        aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
      >
        {collapsed ? '▸▸' : '◂◂'}
      </button>

      <nav className="flex-1 space-y-1">
        {mainSections.map((section) => {
          const isActive = isSectionActive(section)
          const isExpanded = expanded[section.id] ?? true

          if (section.link) {
            return (
              <div key={section.id}>
                <NavLink
                  to={section.link}
                  end
                  className={({ isActive: linkActive }) =>
                    `flex items-center gap-2 rounded-md px-2 py-2 text-sm font-medium transition ${
                      linkActive
                        ? 'bg-primary-subtle text-primary'
                        : 'text-muted hover:bg-panel hover:text-foreground'
                    } ${collapsed ? 'justify-center px-0' : ''}`
                  }
                  title={collapsed ? section.title : undefined}
                >
                  <span className="flex-shrink-0 text-sm">{section.icon}</span>
                  {!collapsed && <span className="truncate">{section.title}</span>}
                </NavLink>
              </div>
            )
          }

          return (
            <div key={section.id}>
              <button
                onClick={() => !collapsed && toggleSection(section.id)}
                className={`flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-xs font-semibold uppercase tracking-wider transition ${
                  isActive && !isExpanded ? 'text-primary' : 'text-dim hover:text-muted'
                } ${collapsed ? 'justify-center px-0' : ''}`}
                title={collapsed ? section.title : undefined}
                aria-label={section.title}
                aria-expanded={isExpanded}
              >
                <span className="flex-shrink-0 text-xs">{section.icon}</span>
                {!collapsed && (
                  <>
                    <span className="flex-1 text-left">{section.title}</span>
                    <span
                      className={`text-[10px] text-dim transition-transform duration-150 ${
                        isExpanded ? 'rotate-0' : '-rotate-90'
                      }`}
                    >
                      ▾
                    </span>
                  </>
                )}
              </button>

              {(isExpanded || collapsed) && (
                <ul
                  className={`space-y-0.5 overflow-hidden transition-all duration-150 ${
                    !collapsed ? 'ml-1 mt-0.5' : 'mt-0.5'
                  }`}
                >
                  {(section.items ?? []).map((item) => (
                    <li key={item.to}>
                      <NavLink
                        to={item.to}
                        className={({ isActive: linkActive }) =>
                          `flex items-center gap-2 rounded-md px-2 py-1.5 text-sm transition ${
                            linkActive
                              ? 'bg-primary-subtle font-medium text-primary'
                              : 'text-muted hover:bg-panel hover:text-foreground'
                          } ${collapsed ? 'justify-center px-0' : ''}`
                        }
                        title={collapsed ? item.label : undefined}
                      >
                        <span className="flex-shrink-0 text-sm">{item.icon}</span>
                        {!collapsed && <span className="truncate">{item.label}</span>}
                      </NavLink>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          )
        })}
      </nav>

      <div className="mt-auto space-y-0.5 border-t border-border pt-2">
        <NavLink
          to="/settings"
          className={({ isActive }) =>
            `flex items-center gap-2 rounded-md px-2 py-1.5 text-sm transition ${
              isActive
                ? 'bg-primary-subtle font-medium text-primary'
                : 'text-muted hover:bg-panel hover:text-foreground'
            } ${collapsed ? 'justify-center px-0' : ''}`
          }
          title={collapsed ? 'Settings' : undefined}
        >
          <span className="flex-shrink-0 text-sm">⚙️</span>
          {!collapsed && <span className="truncate">Settings</span>}
        </NavLink>

        {!collapsed && (
          <div className="px-2 py-1 text-[10px] text-dim">ForgeFleet v2026.4.7</div>
        )}
      </div>
    </aside>
  )
}
