export interface CommandItem {
  id: string
  label: string
  category: string
  path?: string
  action?: () => void
  shortcut?: string
}

export const COMMANDS: CommandItem[] = [
  { id: 'home', label: 'Mission Control', category: 'Navigate', path: '/', shortcut: 'G H' },
  { id: 'pulse', label: 'Pulse', category: 'Navigate', path: '/pulse' },
  { id: 'brain', label: 'Brain / Chat', category: 'Brain', path: '/brain', shortcut: 'G C' },
  { id: 'brain-graph', label: 'Knowledge Graph', category: 'Brain', path: '/brain/graph' },
  { id: 'agents', label: 'Agents / Swarm', category: 'Intelligence', path: '/agents', shortcut: 'G A' },
  { id: 'council', label: 'Council', category: 'Intelligence', path: '/council' },
  { id: 'mcp', label: 'MCP', category: 'Intelligence', path: '/mcp' },
  { id: 'skills', label: 'Skills', category: 'Intelligence', path: '/skills' },
  { id: 'interactions', label: 'Interactions', category: 'Intelligence', path: '/interactions' },
  { id: 'tasks', label: 'My Tasks', category: 'Projects', path: '/my-tasks' },
  { id: 'projects', label: 'Projects', category: 'Projects', path: '/projects', shortcut: 'G P' },
  { id: 'planning', label: 'Planning Hub', category: 'Projects', path: '/planning' },
  { id: 'workflow', label: 'Workflows', category: 'Projects', path: '/workflow' },
  { id: 'fleet', label: 'Fleet Overview', category: 'Fleet', path: '/fleet', shortcut: 'G F' },
  { id: 'topology', label: 'Topology', category: 'Fleet', path: '/topology', shortcut: 'G T' },
  { id: 'mesh', label: 'Mesh Status', category: 'Fleet', path: '/mesh' },
  { id: 'cost-ledger', label: 'Cost Ledger', category: 'Operations', path: '/cost-ledger' },
  { id: 'metrics', label: 'Metrics', category: 'Operations', path: '/metrics' },
  { id: 'alerts', label: 'Alerts', category: 'Operations', path: '/alerts', shortcut: 'G L' },
  { id: 'audit', label: 'Audit Log', category: 'Operations', path: '/audit' },
  { id: 'updates', label: 'Updates', category: 'Operations', path: '/updates' },
  { id: 'versions', label: 'Versions', category: 'Operations', path: '/versions' },
  { id: 'model-hub', label: 'Model Hub', category: 'Models & Tools', path: '/model-hub' },
  { id: 'models', label: 'Model Inventory', category: 'Models & Tools', path: '/models' },
  { id: 'tools', label: 'Tool Inventory', category: 'Models & Tools', path: '/tools', shortcut: 'G K' },
  { id: 'llm-proxy', label: 'LLM Proxy', category: 'Models & Tools', path: '/llm-proxy' },
  { id: 'settings', label: 'Settings', category: 'Admin', path: '/settings', shortcut: 'G S' },
  { id: 'config', label: 'Config Editor', category: 'Admin', path: '/config' },
  { id: 'onboarding', label: 'Onboarding', category: 'Admin', path: '/onboarding' },
  { id: 'refresh', label: 'Refresh Page', category: 'Action', action: () => window.location.reload() },
]

export const COMMAND_CATEGORIES = Array.from(new Set(COMMANDS.map((c) => c.category)))
