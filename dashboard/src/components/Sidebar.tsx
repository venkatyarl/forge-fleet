import { NavLink } from 'react-router-dom'

type NavItem = {
  to: string
  label: string
}

type NavSection = {
  title: string
  items: NavItem[]
}

const navSections: NavSection[] = [
  {
    title: 'Fleet',
    items: [
      { to: '/', label: 'Fleet Overview' },
      { to: '/topology', label: 'Topology' },
      { to: '/models', label: 'Model Inventory' },
      { to: '/metrics', label: 'Metrics' },
    ],
  },
  {
    title: 'Operations',
    items: [
      { to: '/mission-control', label: 'Mission Control' },
      { to: '/onboarding', label: 'Operator Onboarding' },
      { to: '/my-tasks', label: 'My Tasks' },
      { to: '/workflow', label: 'Workflow Workbench' },
      { to: '/planning', label: 'Planning Hub' },
      { to: '/projects', label: 'Projects' },
      { to: '/chats', label: 'Chats' },
      { to: '/chat', label: 'Chat Studio' },
      { to: '/updates', label: 'Updates' },
      { to: '/audit', label: 'Audit Log' },
    ],
  },
  {
    title: 'Configuration',
    items: [
      { to: '/settings', label: 'Settings' },
      { to: '/config', label: 'Config Editor' },
      { to: '/llm-proxy', label: 'LLM Proxy' },
    ],
  },
]

export function Sidebar() {
  return (
    <aside className="w-full border-b border-slate-800 bg-slate-900/50 p-3 md:w-72 md:border-b-0 md:border-r md:p-4">
      <div className="mb-3 px-1">
        <p className="text-[11px] uppercase tracking-wider text-slate-500">Navigation</p>
        <p className="text-xs text-slate-400">Fleet state, operations, and runtime configuration</p>
      </div>

      <nav className="space-y-4">
        {navSections.map((section) => (
          <section key={section.title} className="space-y-2">
            <h2 className="px-1 text-[11px] font-semibold uppercase tracking-wider text-slate-500">
              {section.title}
            </h2>
            <div className="grid grid-cols-2 gap-2 md:grid-cols-1">
              {section.items.map((item) => (
                <NavLink
                  key={item.to}
                  to={item.to}
                  end={item.to === '/'}
                  className={({ isActive }) =>
                    `rounded-md px-3 py-2 text-sm transition ${
                      isActive
                        ? 'bg-sky-500/20 text-sky-300'
                        : 'text-slate-300 hover:bg-slate-800 hover:text-white'
                    }`
                  }
                >
                  {item.label}
                </NavLink>
              ))}
            </div>
          </section>
        ))}
      </nav>
    </aside>
  )
}
