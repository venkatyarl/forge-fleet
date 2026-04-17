import { useState, useEffect } from 'react'
import { NavLink, useLocation } from 'react-router-dom'

type NavItem = { to: string; label: string; icon: string }
type NavSection = {
  id: string
  title: string
  icon: string
  items: NavItem[]
  /** If set, the section title itself is a link (no sub-items toggle) */
  link?: string
}

const mainSections: NavSection[] = [
  {
    id: 'mission-control',
    title: 'Mission Control',
    icon: '🎯',
    link: '/',
    items: [],
  },
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
    id: 'chats',
    title: 'Chats',
    icon: '💬',
    items: [
      { to: '/chat', label: 'New Chat', icon: '✨' },
      { to: '/chats', label: 'Chat History', icon: '📝' },
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
]

function loadExpandedState(): Record<string, boolean> {
  try {
    const raw = localStorage.getItem('ff_sidebar_expanded')
    if (raw) return JSON.parse(raw)
  } catch { /* ignore */ }
  return Object.fromEntries(mainSections.filter(s => s.items.length > 0).map(s => [s.id, true]))
}

export function Sidebar() {
  const [collapsed, setCollapsed] = useState(false)
  const [expanded, setExpanded] = useState<Record<string, boolean>>(loadExpandedState)
  const location = useLocation()

  useEffect(() => {
    localStorage.setItem('ff_sidebar_expanded', JSON.stringify(expanded))
  }, [expanded])

  const toggleSection = (id: string) => {
    setExpanded(prev => ({ ...prev, [id]: !prev[id] }))
  }

  const isSectionActive = (section: NavSection) => {
    if (section.link) return location.pathname === section.link
    return section.items.some(item => location.pathname === item.to || location.pathname.startsWith(item.to + '/'))
  }

  return (
    <aside className={`flex-shrink-0 border-b border-zinc-800 bg-[#18181B]/80 transition-all duration-200 md:border-b-0 md:border-r ${
      collapsed ? 'md:w-14' : 'w-full md:w-56'
    } flex h-full flex-col overflow-y-auto p-2`}>

      <button
        onClick={() => setCollapsed(!collapsed)}
        className="mb-2 hidden w-full rounded p-1 text-xs text-zinc-600 hover:bg-zinc-800 hover:text-zinc-400 md:block"
        title={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
      >
        {collapsed ? '▸▸' : '◂◂'}
      </button>

      {/* Main navigation — grows to fill space */}
      <nav className="flex-1 space-y-1">
        {mainSections.map((section) => {
          const isActive = isSectionActive(section)
          const isExpanded = expanded[section.id] ?? true

          // Direct link section (Mission Control)
          if (section.link) {
            return (
              <div key={section.id}>
                <NavLink
                  to={section.link}
                  end
                  className={({ isActive: linkActive }) =>
                    `flex items-center gap-2 rounded-md px-2 py-2 text-sm font-medium transition ${
                      linkActive
                        ? 'bg-violet-500/15 text-violet-300'
                        : 'text-zinc-300 hover:bg-zinc-800/70 hover:text-zinc-100'
                    } ${collapsed ? 'justify-center px-0' : ''}`
                  }
                  title={collapsed ? section.title : undefined}
                >
                  <span className="text-sm flex-shrink-0">{section.icon}</span>
                  {!collapsed && <span className="truncate">{section.title}</span>}
                </NavLink>
              </div>
            )
          }

          // Collapsible section
          return (
            <div key={section.id}>
              <button
                onClick={() => !collapsed && toggleSection(section.id)}
                className={`flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-xs font-semibold uppercase tracking-wider transition ${
                  isActive && !isExpanded
                    ? 'text-violet-400'
                    : 'text-zinc-500 hover:text-zinc-300'
                } ${collapsed ? 'justify-center px-0' : ''}`}
                title={collapsed ? section.title : undefined}
              >
                <span className="text-xs flex-shrink-0">{section.icon}</span>
                {!collapsed && (
                  <>
                    <span className="flex-1 text-left">{section.title}</span>
                    <span className={`text-[10px] text-zinc-600 transition-transform duration-150 ${isExpanded ? 'rotate-0' : '-rotate-90'}`}>
                      ▾
                    </span>
                  </>
                )}
              </button>

              {(isExpanded || collapsed) && (
                <ul className={`space-y-0.5 overflow-hidden transition-all duration-150 ${!collapsed ? 'ml-1 mt-0.5' : 'mt-0.5'}`}>
                  {section.items.map((item) => (
                    <li key={item.to}>
                      <NavLink
                        to={item.to}
                        className={({ isActive: linkActive }) =>
                          `flex items-center gap-2 rounded-md px-2 py-1.5 text-sm transition ${
                            linkActive
                              ? 'bg-violet-500/15 text-violet-300 font-medium'
                              : 'text-zinc-400 hover:bg-zinc-800/70 hover:text-zinc-200'
                          } ${collapsed ? 'justify-center px-0' : ''}`
                        }
                        title={collapsed ? item.label : undefined}
                      >
                        <span className="text-sm flex-shrink-0">{item.icon}</span>
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

      {/* Bottom section — pinned to bottom */}
      <div className="mt-auto border-t border-zinc-800 pt-2 space-y-0.5">
        <NavLink
          to="/settings"
          className={({ isActive }) =>
            `flex items-center gap-2 rounded-md px-2 py-1.5 text-sm transition ${
              isActive
                ? 'bg-violet-500/15 text-violet-300 font-medium'
                : 'text-zinc-400 hover:bg-zinc-800/70 hover:text-zinc-200'
            } ${collapsed ? 'justify-center px-0' : ''}`
          }
          title={collapsed ? 'Settings' : undefined}
        >
          <span className="text-sm flex-shrink-0">⚙️</span>
          {!collapsed && <span className="truncate">Settings</span>}
        </NavLink>

        {!collapsed && (
          <div className="px-2 py-1 text-[10px] text-zinc-600">
            ForgeFleet v2026.4.7
          </div>
        )}
      </div>
    </aside>
  )
}
