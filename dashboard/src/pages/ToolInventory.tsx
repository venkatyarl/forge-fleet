import { useEffect, useMemo, useState } from 'react'

// Static fallback catalog — shown when the backend tool registry is empty
// (e.g. during development or before agents have registered their tools).
const STATIC_TOOLS: StaticToolInfo[] = [
  { name: 'Bash', category: 'File Ops', description: 'Execute shell commands with persistent state' },
  { name: 'Read', category: 'File Ops', description: 'Read files with line numbers, offset/limit' },
  { name: 'Write', category: 'File Ops', description: 'Create or overwrite files' },
  { name: 'Edit', category: 'File Ops', description: 'Exact string replacement in files' },
  { name: 'Glob', category: 'File Ops', description: 'Find files by pattern (e.g. **/*.rs)' },
  { name: 'Grep', category: 'File Ops', description: 'Search file contents with regex' },
  { name: 'Agent', category: 'Agent', description: 'Spawn sub-agents on fleet nodes' },
  { name: 'SendMessage', category: 'Agent', description: 'Inter-agent messaging' },
  { name: 'Delegate', category: 'Agent', description: 'Route subtask to specialized agent role' },
  { name: 'TaskCreate', category: 'Tasks', description: 'Create a task to track work' },
  { name: 'TaskGet', category: 'Tasks', description: 'Get task details by ID' },
  { name: 'TaskUpdate', category: 'Tasks', description: 'Update task status or details' },
  { name: 'TaskList', category: 'Tasks', description: 'List all tasks' },
  { name: 'TaskStop', category: 'Tasks', description: 'Cancel a running task' },
  { name: 'TaskOutput', category: 'Tasks', description: 'Get task output/result' },
  { name: 'WebFetch', category: 'Web', description: 'Fetch web pages and convert to text' },
  { name: 'WebSearch', category: 'Web', description: 'Search the web via DuckDuckGo' },
  { name: 'HttpRequest', category: 'Web', description: 'Generic HTTP client (GET/POST/PUT/DELETE)' },
  { name: 'DeepResearch', category: 'Research', description: 'Multi-source research with summarization' },
  { name: 'WikiLookup', category: 'Research', description: 'Wikipedia article lookup' },
  { name: 'ScholarSearch', category: 'Research', description: 'Academic paper search (Semantic Scholar)' },
  { name: 'GitPR', category: 'Git', description: 'GitHub PR management (create/list/merge/review)' },
  { name: 'GitBlame', category: 'Git', description: 'Git blame analysis with porcelain parsing' },
  { name: 'GithubIssues', category: 'Git', description: 'Create/list/manage GitHub issues' },
  { name: 'TestGen', category: 'Git', description: 'Extract code for test generation' },
  { name: 'EnterWorktree', category: 'Git', description: 'Create isolated git worktree' },
  { name: 'ExitWorktree', category: 'Git', description: 'Remove git worktree' },
  { name: 'Docker', category: 'DevOps', description: 'Container management (ps/build/run/compose)' },
  { name: 'LintFix', category: 'DevOps', description: 'Run linter/formatter/tests with auto-fix' },
  { name: 'DocGen', category: 'DevOps', description: 'Generate documentation (rustdoc/JSDoc)' },
  { name: 'DepCheck', category: 'DevOps', description: 'Audit dependencies for vulnerabilities' },
  { name: 'CronSchedule', category: 'DevOps', description: 'Schedule recurring fleet tasks' },
  { name: 'ProjectEstimate', category: 'Project Mgmt', description: 'Story points and hour estimates from descriptions' },
  { name: 'VelocityTracker', category: 'Project Mgmt', description: 'Calculate team velocity from sprint history' },
  { name: 'DeadlineProjector', category: 'Project Mgmt', description: 'Project completion date from remaining work' },
  { name: 'SprintPlanner', category: 'Project Mgmt', description: 'Auto-assign items to sprint by priority/capacity' },
  { name: 'RiskAssessor', category: 'Project Mgmt', description: 'Identify blocked items, scope creep, bottlenecks' },
  { name: 'WorkloadBalancer', category: 'Project Mgmt', description: 'Distribute work evenly across assignees' },
  { name: 'DependencyMapper', category: 'Project Mgmt', description: 'Analyze dependency chains, find critical path' },
  { name: 'BudgetTracker', category: 'Finance', description: 'Income/expense tracking with category breakdown' },
  { name: 'ProfitLoss', category: 'Finance', description: 'P&L statement (revenue, COGS, net income)' },
  { name: 'CashFlowForecast', category: 'Finance', description: 'Project N months of cash flow' },
  { name: 'InvoiceGen', category: 'Finance', description: 'Generate professional invoices' },
  { name: 'StatsCalc', category: 'Analytics', description: 'Mean, median, std dev, percentiles, correlation' },
  { name: 'TimeSeriesAnalysis', category: 'Analytics', description: 'Trend detection, moving averages, outliers' },
  { name: 'NodeSetup', category: 'Fleet Ops', description: 'Install prerequisites on new machines via SSH' },
  { name: 'NodeEnroll', category: 'Fleet Ops', description: 'Register node in fleet.toml' },
  { name: 'ModelDeploy', category: 'Fleet Ops', description: 'Download and deploy models to fleet nodes' },
  { name: 'FleetInventory', category: 'Fleet Ops', description: 'Scan fleet, report all nodes and models' },
  { name: 'NodeHealthCheck', category: 'Fleet Ops', description: 'Deep health check via SSH' },
  { name: 'BinaryDeploy', category: 'Fleet Ops', description: 'Build and deploy ForgeFleet binary to nodes' },
  { name: 'PatternLearner', category: 'Intelligence', description: 'Track successful patterns per task type' },
  { name: 'ModelScorecard', category: 'Intelligence', description: 'Track model quality, generate leaderboards' },
  { name: 'ReviewQueue', category: 'Intelligence', description: 'Queue work for human review' },
  { name: 'RollbackManager', category: 'Intelligence', description: 'Preview/stash/rollback git changes' },
  { name: 'SmartSearch', category: 'Intelligence', description: 'Search across code, memory, git, docs' },
  { name: 'WatchAndReact', category: 'Intelligence', description: 'Event-driven triggers for agent tasks' },
  { name: 'ProjectScaffold', category: 'Intelligence', description: 'Generate new projects from templates' },
  { name: 'Screenshot', category: 'Media', description: 'Capture web page screenshots' },
  { name: 'ImageAnalyze', category: 'Media', description: 'Image dimensions, EXIF, OCR' },
  { name: 'VideoDownload', category: 'Media', description: 'Download videos via yt-dlp' },
  { name: 'LinkPreview', category: 'Media', description: 'Fetch OpenGraph metadata from URLs' },
  { name: 'ImageConvert', category: 'Media', description: 'Resize, convert, compress images' },
  { name: 'PhotoAnalysis', category: 'Multimodal', description: 'Full photo analysis (OCR, EXIF, colors)' },
  { name: 'VideoAnalysis', category: 'Multimodal', description: 'Video metadata, frame extraction, transcription' },
  { name: 'AudioAnalysis', category: 'Multimodal', description: 'Audio transcription (Whisper), conversion' },
  { name: 'ProcessManager', category: 'Computer', description: 'List/search/kill processes' },
  { name: 'Clipboard', category: 'Computer', description: 'Read/write system clipboard' },
  { name: 'SystemControl', category: 'Computer', description: 'Open apps/URLs, notifications, system info' },
  { name: 'ServiceManager', category: 'Computer', description: 'Manage system services (systemd/launchd)' },
  { name: 'PackageManager', category: 'Computer', description: 'Install/update system packages' },
  { name: 'DatabaseQuery', category: 'Database', description: 'Run SQL against PostgreSQL/SQLite/MySQL' },
  { name: 'HashGenerator', category: 'Crypto', description: 'SHA256/SHA512/MD5 for strings and files' },
  { name: 'PasswordGen', category: 'Crypto', description: 'Secure random passwords and passphrases' },
  { name: 'TextTransform', category: 'Crypto', description: 'Base64, URL encode/decode, JSON format' },
  { name: 'Calculator', category: 'Crypto', description: 'Evaluate math expressions' },
  { name: 'ModelBrowser', category: 'Models', description: 'Search HuggingFace, Ollama, fleet models' },
  { name: 'ModelDownloader', category: 'Models', description: 'Download models (Ollama/HF/URL)' },
  { name: 'ModelCompare', category: 'Models', description: 'Side-by-side model comparison' },
  { name: 'ModelDiscovery', category: 'Models', description: 'Discover models from all sources' },
  { name: 'ClusterInference', category: 'Models', description: 'Distributed inference across fleet nodes' },
  { name: 'VersionManager', category: 'Version', description: 'Version management, upgrades, fleet deploy' },
  { name: 'Reminder', category: 'Utility', description: 'Set time-based reminders' },
  { name: 'Timer', category: 'Utility', description: 'Benchmark command execution time' },
  { name: 'Regex', category: 'Utility', description: 'Test and debug regex patterns' },
  { name: 'Diagram', category: 'Utility', description: 'Generate Mermaid diagrams' },
  { name: 'Diff', category: 'Utility', description: 'Generate diffs (files, git versions)' },
  { name: 'JsonQuery', category: 'Utility', description: 'Query JSON with jq expressions' },
  { name: 'FileCompress', category: 'Utility', description: 'Zip/tar compress and decompress' },
  { name: 'FileSync', category: 'Utility', description: 'Rsync between local and fleet nodes' },
  { name: 'HealthMonitor', category: 'Utility', description: 'Check URL health with timing' },
  { name: 'SelfHeal', category: 'Automation', description: 'Diagnose and auto-fix fleet failures' },
  { name: 'AutoFleet', category: 'Automation', description: 'Autonomous fleet management' },
  { name: 'TaskDecomposer', category: 'Automation', description: 'Break complex tasks into subtrees' },
  { name: 'ToolBuilder', category: 'Builders', description: 'Create new compiled Rust tools at runtime' },
  { name: 'SkillBuilder', category: 'Builders', description: 'Create loadable SKILL.md skills at runtime' },
  { name: 'AskUserQuestion', category: 'Planning', description: 'Request user input/clarification' },
  { name: 'EnterPlanMode', category: 'Planning', description: 'Switch to read-only planning mode' },
  { name: 'ExitPlanMode', category: 'Planning', description: 'Exit planning, start implementing' },
  { name: 'VerifyAndRetry', category: 'Agentic', description: 'Run verification, report pass/fail' },
  { name: 'PdfExtract', category: 'Agentic', description: 'Extract text from PDFs' },
  { name: 'SpreadsheetQuery', category: 'Agentic', description: 'Read/query CSV and Excel files' },
  { name: 'ChangelogGen', category: 'Content', description: 'Generate changelogs from git history' },
  { name: 'ReportGen', category: 'Content', description: 'Generate structured markdown reports' },
  { name: 'MeetingNotes', category: 'Content', description: 'Structure notes into action items' },
  { name: 'CodeComplexity', category: 'Code Quality', description: 'Analyze code complexity and file sizes' },
  { name: 'DuplicateDetector', category: 'Code Quality', description: 'Find duplicate code patterns' },
  { name: 'LogAnalyzer', category: 'Code Quality', description: 'Parse and analyze log files' },
]

type StaticToolInfo = {
  name: string
  category: string
  description: string
}

type LiveTool = {
  tool_name: string
  worker_name: string
  description: string
  health_checked_at: string
  call_count: number
  avg_latency_ms: number | null
  healthy: boolean
}

async function fetchTools(): Promise<LiveTool[]> {
  try {
    const res = await fetch('/api/tools')
    if (!res.ok) return []
    const data = await res.json()
    return Array.isArray(data.tools) ? data.tools : []
  } catch {
    return []
  }
}

function staticToLive(t: StaticToolInfo): LiveTool {
  return {
    tool_name: t.name,
    worker_name: 'fleet',
    description: t.description,
    health_checked_at: new Date().toISOString(),
    call_count: 0,
    avg_latency_ms: null,
    healthy: true,
  }
}

export function ToolInventory() {
  const [liveTools, setLiveTools] = useState<LiveTool[] | null>(null)
  const [search, setSearch] = useState('')
  const [selectedCategory, setSelectedCategory] = useState<string | null>(null)

  useEffect(() => {
    fetchTools().then(setLiveTools)
  }, [])

  const tools = useMemo<LiveTool[]>(() => {
    if (liveTools === null) return []
    if (liveTools.length > 0) return liveTools
    return STATIC_TOOLS.map(staticToLive)
  }, [liveTools])

  const isFallback = liveTools !== null && liveTools.length === 0

  const filtered = useMemo(() => {
    return tools.filter(t => {
      const matchesSearch = !search
        || t.tool_name.toLowerCase().includes(search.toLowerCase())
        || t.description.toLowerCase().includes(search.toLowerCase())
      const matchesCategory = !selectedCategory || t.worker_name === selectedCategory
      return matchesSearch && matchesCategory
    })
  }, [tools, search, selectedCategory])

  const categories = useMemo(() => {
    const cats = new Set<string>()
    tools.forEach(t => cats.add(t.worker_name))
    return [...cats].sort()
  }, [tools])

  const categoryCounts = useMemo(() => {
    const counts: Record<string, number> = {}
    tools.forEach(t => { counts[t.worker_name] = (counts[t.worker_name] || 0) + 1 })
    return counts
  }, [tools])

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-semibold text-slate-100">Tool Inventory</h2>
          <p className="text-sm text-slate-400">
            {tools.length} tool{tools.length !== 1 ? 's' : ''} across {categories.length} node{categories.length !== 1 ? 's' : ''}
            {isFallback && ' (static fallback — no live registry data)'}
          </p>
        </div>
        <input
          type="text"
          placeholder="Search tools..."
          value={search}
          onChange={e => setSearch(e.target.value)}
          className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-100 w-64"
        />
      </div>

      <div className="flex gap-2 flex-wrap">
        <button
          onClick={() => setSelectedCategory(null)}
          className={`rounded-full px-3 py-1 text-xs ${!selectedCategory ? 'bg-violet-500/30 text-violet-200 border border-violet-500/50' : 'bg-slate-800 text-slate-400 border border-slate-700'}`}
        >
          All ({tools.length})
        </button>
        {categories.map(cat => (
          <button
            key={cat}
            onClick={() => setSelectedCategory(selectedCategory === cat ? null : cat)}
            className={`rounded-full px-3 py-1 text-xs ${selectedCategory === cat ? 'bg-violet-500/30 text-violet-200 border border-violet-500/50' : 'bg-slate-800 text-slate-400 border border-slate-700'}`}
          >
            {cat} ({categoryCounts[cat] || 0})
          </button>
        ))}
      </div>

      <div className="grid gap-3 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
        {filtered.map(tool => (
          <article key={`${tool.tool_name}-${tool.worker_name}`} className="rounded-xl border border-slate-800 bg-slate-900/70 p-4 hover:border-slate-600 transition">
            <div className="flex items-start justify-between">
              <h3 className="font-mono font-semibold text-emerald-400">{tool.tool_name}</h3>
              <div className="flex items-center gap-1.5">
                {tool.healthy !== undefined && (
                  <span className={`inline-block h-2 w-2 rounded-full ${tool.healthy ? 'bg-emerald-500' : 'bg-red-500'}`} title={tool.healthy ? 'Healthy' : 'Unhealthy'} />
                )}
                <span className="rounded-full bg-slate-800 px-2 py-0.5 text-xs text-slate-400">{tool.worker_name}</span>
              </div>
            </div>
            <p className="mt-2 text-sm text-slate-400">{tool.description}</p>
            {(tool.call_count > 0 || tool.avg_latency_ms != null) && (
              <div className="mt-2 flex gap-3 text-xs text-slate-500">
                {tool.call_count > 0 && <span>{tool.call_count.toLocaleString()} calls</span>}
                {tool.avg_latency_ms != null && <span>{Math.round(tool.avg_latency_ms)}ms avg</span>}
              </div>
            )}
          </article>
        ))}
      </div>

      {filtered.length === 0 && (
        <div className="rounded-xl border border-slate-800 bg-slate-900/70 p-8 text-center text-slate-500">
          No tools matching "{search}" {selectedCategory ? `in ${selectedCategory}` : ''}
        </div>
      )}
    </section>
  )
}
