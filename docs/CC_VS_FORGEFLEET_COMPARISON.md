# Claude Code vs ForgeFleet — Full Feature Comparison

## Legend
- **CC** = Claude Code (TypeScript/Node.js, ~/Downloads/CC_sources/anthropic-leaked-source-code-main/)
- **FF** = ForgeFleet (Rust, ~/projects/forge-fleet/)
- ✅ = Fully implemented
- 🟡 = Partially implemented / scaffolded
- ❌ = Missing — needs to be built

---

## 1. TOOLS (CC: 55 tools → FF: 6 tools)

### Core File & Shell Tools

| Tool | CC | FF | Gap |
|------|----|----|-----|
| **BashTool** — shell execution with state persistence | ✅ Full (cwd/env tracking, classifier, timeout, background) | ✅ Basic (cwd via wrapper, timeout, blocked commands) | FF missing: persistent cwd/env across calls, bash classifier for risky commands, background mode |
| **FileReadTool** — read files with line numbers | ✅ Full (images, PDFs, notebooks, binary detection) | ✅ Basic (text files, offset/limit, binary detection) | FF missing: image rendering, PDF page support, Jupyter notebook parsing |
| **FileEditTool** — exact string replacement | ✅ Full (old→new, replace_all, file history tracking) | ✅ Basic (old→new, replace_all) | FF missing: file change history/tracking for undo |
| **FileWriteTool** — create/overwrite files | ✅ Full (parent mkdir, before/after tracking) | ✅ Basic (parent mkdir) | FF missing: before/after diff tracking |
| **GlobTool** — file pattern matching | ✅ Full (sorted by mtime, skip hidden, 250 limit) | ✅ Full (same features) | Parity ✅ |
| **GrepTool** — regex content search | ✅ Full (rg backend, type filter, context, multiline) | ✅ Full (rg backend, type filter, context) | FF missing: multiline mode flag |
| **NotebookEditTool** — Jupyter notebook editing | ✅ Full (cell insert/replace/delete) | ❌ Missing | Need to add |
| **PowerShellTool** — Windows shell | ✅ Feature-gated | ❌ Missing | Low priority (fleet is Mac/Linux) |

### Agent & Task Tools

| Tool | CC | FF | Gap |
|------|----|----|-----|
| **AgentTool** — spawn sub-agents | ✅ Full (isolation, model override, worktree) | ❌ Missing | **HIGH PRIORITY** — need sub-agent spawning |
| **SendMessageTool** — inter-agent messaging | ✅ Full (agent-to-agent communication) | ❌ Missing | Need for multi-agent coordination |
| **TeamCreateTool** — create agent teams | ✅ Full (swarm management) | ❌ Missing | Maps to ff-orchestrator crew system |
| **TeamDeleteTool** — delete agent teams | ✅ Full | ❌ Missing | Paired with TeamCreate |
| **TaskCreateTool** — create background tasks | ✅ Full (subject, description, metadata) | ❌ Missing | Need for background task management |
| **TaskGetTool** — get task details | ✅ Full | ❌ Missing | |
| **TaskUpdateTool** — update task status | ✅ Full | ❌ Missing | |
| **TaskListTool** — list all tasks | ✅ Full | ❌ Missing | |
| **TaskStopTool** — stop background task | ✅ Full | ❌ Missing | |
| **TaskOutputTool** — get task output | ✅ Full | ❌ Missing | |

### Web & Search Tools

| Tool | CC | FF | Gap |
|------|----|----|-----|
| **WebFetchTool** — fetch web pages | ✅ Full (HTML→text, JS rendering) | ❌ Missing | Need to add |
| **WebSearchTool** — web search | ✅ Full (max 8 results, domain filter) | ❌ Missing | Need to add |
| **WebBrowserTool** — browser automation | ✅ Feature-gated | ❌ Missing | Low priority |

### Planning & Mode Tools

| Tool | CC | FF | Gap |
|------|----|----|-----|
| **EnterPlanModeTool** — enter plan mode | ✅ Full | ❌ Missing | Need for planning workflow |
| **ExitPlanModeTool** — exit plan mode | ✅ Full | ❌ Missing | |
| **AskUserQuestionTool** — request user input | ✅ Full (interactive prompt) | ❌ Missing | **HIGH PRIORITY** for interactive sessions |
| **SkillTool** — invoke skills | ✅ Full (skill invocation as subagent) | 🟡 ff-skills has executor but not as agent tool | Wire skill executor as agent tool |
| **ToolSearchTool** — search deferred tools | ✅ Full (lazy loading) | ❌ Missing | Need for large tool registries |
| **BriefTool** — get context briefing | ✅ Feature-gated (KAIROS) | ❌ Missing | |

### Git & Workspace Tools

| Tool | CC | FF | Gap |
|------|----|----|-----|
| **EnterWorktreeTool** — create git worktree | ✅ Full (isolated copy of repo) | ❌ Missing | Need for safe parallel work |
| **ExitWorktreeTool** — exit worktree | ✅ Full | ❌ Missing | |

### MCP Tools

| Tool | CC | FF | Gap |
|------|----|----|-----|
| **ListMcpResourcesTool** — list MCP resources | ✅ Full | ❌ Missing as agent tool | FF has MCP server but not client-side tools |
| **ReadMcpResourceTool** — read MCP resource | ✅ Full | ❌ Missing as agent tool | |
| **MCPTool** — dynamic MCP tool wrapper | ✅ Full (per-server) | ❌ Missing | Need MCP client in agent loop |

### Advanced / Feature-Gated Tools

| Tool | CC | FF | Gap |
|------|----|----|-----|
| **SleepTool** — pause execution | ✅ Feature-gated | ❌ Missing | Trivial to add |
| **CronCreateTool** — schedule jobs | ✅ Feature-gated | ❌ as agent tool | FF has ff-cron but not exposed as tool |
| **CronDeleteTool** — delete cron | ✅ Feature-gated | ❌ as agent tool | |
| **CronListTool** — list cron | ✅ Feature-gated | ❌ as agent tool | |
| **RemoteTriggerTool** — trigger remote agents | ✅ Feature-gated | ❌ Missing | Maps to ff-mesh remote dispatch |
| **MonitorTool** — monitor operations | ✅ Feature-gated | ❌ Missing | |
| **LSPTool** — language server protocol | ✅ Full | ❌ Missing | Advanced, lower priority |
| **REPLTool** — JavaScript REPL | ✅ Ant-only | ❌ Missing | Could do Rust REPL or generic |
| **ConfigTool** — manage config | ✅ Ant-only | ❌ Missing | |
| **ComputerUseTool** — computer use | ✅ In source | ❌ Missing | |
| **TodoWriteTool** — legacy task list | ✅ Legacy | ❌ Missing | Low priority (superseded by Task tools) |

**TOOL SCORE: FF has 6/55 tools (11%). Need ~25 more for functional parity.**

---

## 2. SLASH COMMANDS (CC: 80+ → FF: 0)

| Command | CC | FF | Priority |
|---------|----|----|----------|
| `/help` | ✅ | ❌ | HIGH |
| `/clear` | ✅ | ❌ | HIGH |
| `/compact` | ✅ Context compaction | ❌ | HIGH — needed for long sessions |
| `/model` | ✅ Switch model | ❌ | HIGH — switch fleet LLM |
| `/cost` | ✅ Token/cost tracking | ❌ | MEDIUM |
| `/plan` | ✅ Enter plan mode | ❌ | HIGH |
| `/diff` | ✅ Show git diff | ❌ | MEDIUM |
| `/memory` | ✅ Edit CLAUDE.md | ❌ | MEDIUM — maps to ff-memory |
| `/resume` | ✅ Resume session | ❌ | HIGH — session persistence |
| `/export` | ✅ Export conversation | ❌ | MEDIUM |
| `/permissions` | ✅ View/set permissions | ❌ | MEDIUM |
| `/vim` | ✅ Toggle vim mode | ❌ | LOW |
| `/theme` | ✅ Color theme | ❌ | LOW |
| `/fast` | ✅ Toggle fast mode | ❌ | MEDIUM — switch to faster model |
| `/tasks` | ✅ Show task list | ❌ | HIGH |
| `/skills` | ✅ List skills | ❌ | MEDIUM |
| `/mcp` | ✅ Manage MCP servers | ❌ | MEDIUM |
| `/hooks` | ✅ Manage hooks | ❌ | MEDIUM |
| `/config` | ✅ Edit settings | ❌ | MEDIUM |
| `/status` | ✅ System status | ❌ | HIGH — fleet status |
| `/init` | ✅ Initialize project | ❌ | MEDIUM |
| `/doctor` | ✅ Diagnostics | ❌ | MEDIUM |
| `/login` / `/logout` | ✅ Auth management | ❌ | LOW (local LLMs) |
| `/commit` | ✅ Git commit | ❌ | MEDIUM |
| `/review` | ✅ Code review | ❌ | MEDIUM |
| `/branch` | ✅ Git branch/worktree | ❌ | MEDIUM |
| `/add-dir` | ✅ Add directory context | ❌ | MEDIUM |
| `/rewind` | ✅ Undo last turn | ❌ | HIGH |
| `/buddy` | ✅ Feature-gated pet | ❌ | LOW (fun) |
| `/voice` | ✅ Feature-gated voice | 🟡 ff-voice exists | Wire to CLI |
| `/bridge` | ✅ Feature-gated | ❌ | LOW |
| `/ultraplan` | ✅ Feature-gated | ❌ | LOW |
| Others (40+) | ✅ | ❌ | Various |

**COMMAND SCORE: FF has 0/80+ commands. Need ~15 high-priority ones first.**

---

## 3. SYSTEM PROMPT ARCHITECTURE

| Feature | CC | FF | Gap |
|---------|----|----|-----|
| **Modular system prompt** | ✅ Multiple sections with cache boundaries | 🟡 Single string in agent_loop.rs | Need modular prompt builder |
| **Dynamic context injection** | ✅ Git status, CLAUDE.md, project files | ❌ | Need project context injection |
| **CLAUDE.md / memory files** | ✅ Hierarchical (cwd → root → home) | ❌ | Need equivalent (FORGEFLEET.md?) |
| **Prompt caching** | ✅ Cache control headers, boundary markers | ❌ | N/A for local LLMs (no prompt cache API) |
| **System reminders** | ✅ Injected between turns | ❌ | Need turn-level context injection |
| **Tool descriptions in prompt** | ✅ Dynamically assembled | ✅ Via OpenAI tools field | Parity ✅ |
| **Effort level** | ✅ Low/medium/high thinking budget | ❌ | Need effort-based temperature/token tuning |
| **Output style** | ✅ Concise/normal/verbose | ❌ | Need output style control |

---

## 4. AGENT LOOP & QUERY ENGINE

| Feature | CC | FF | Gap |
|---------|----|----|-----|
| **Think→Tool→Observe cycle** | ✅ Full (streaming, parallel tools) | ✅ Basic (non-streaming, sequential) | FF needs: streaming, parallel tool exec |
| **Auto-compaction** | ✅ Reactive + proactive at 90% context | ❌ | **HIGH PRIORITY** — needed for long sessions |
| **Tool-result budgeting** | ✅ 50K char budget, oldest truncated first | ❌ | Need to add |
| **Max turns** | ✅ Configurable | ✅ Configurable (default 30) | Parity ✅ |
| **Cancellation** | ✅ CancellationToken | ✅ CancellationToken | Parity ✅ |
| **Cost/token tracking** | ✅ Atomic counters, USD budget | ❌ | Need usage tracking |
| **Token warnings** | ✅ 80% warning, 95% critical | ❌ | Need context window monitoring |
| **Streaming responses** | ✅ SSE streaming to TUI | ❌ Non-streaming only | Need SSE streaming support |
| **Parallel tool execution** | ✅ futures::join_all | ❌ Sequential only | Need parallel execution |
| **Sub-agent spawning** | ✅ Isolated child query loops | ❌ | **HIGH PRIORITY** |
| **Hooks (PreToolUse, etc.)** | ✅ 6 hook events | ❌ | Need hook system |
| **Session persistence** | ✅ JSONL conversation history | ❌ | Need save/resume |
| **Session branching** | ✅ Fork conversation at any point | ❌ | |
| **Auto-dream / memory extraction** | ✅ Background memory consolidation | ❌ | Maps to ff-memory |

---

## 5. PERMISSION & SECURITY MODEL

| Feature | CC | FF | Gap |
|---------|----|----|-----|
| **Permission modes** | ✅ 4 modes (Default, AcceptEdits, Bypass, Plan) | 🟡 ff-security has autonomy policy | Need per-tool permission modes |
| **Per-tool permission levels** | ✅ None/ReadOnly/Write/Execute/Dangerous/Forbidden | 🟡 AgentTool trait exists but no levels | Need permission levels on tools |
| **Interactive permission prompts** | ✅ TUI dialog, allow/deny/permanent | ❌ | Need for UI agent sessions |
| **Path-based restrictions** | ✅ Blocked paths (.env, credentials) | ❌ | Need blocked file patterns |
| **Command classification** | ✅ ML-based bash classifier | 🟡 Simple keyword-based | Upgrade to smarter classifier |
| **Sandbox mode** | ✅ Container-based sandbox | 🟡 ff-security has sandbox placeholder | Need real sandboxing |
| **API key auth** | ✅ Multiple providers | 🟡 ff-security has API key scopes | N/A for local LLMs |
| **Secret detection** | ✅ Detects secrets in output | ❌ | Need to add |

---

## 6. MCP (Model Context Protocol)

| Feature | CC | FF | Gap |
|---------|----|----|-----|
| **MCP Client** | ✅ Connect to external MCP servers | ❌ | **HIGH PRIORITY** — FF only has MCP server |
| **MCP Server** | ❌ CC is a client only | ✅ 16 tools exposed | FF has server, CC doesn't |
| **Transports** | ✅ Stdio, HTTP/SSE | ✅ Stdio, HTTP | Parity ✅ |
| **Tool discovery** | ✅ tools/list → register | ❌ client-side | Need MCP client tool discovery |
| **Resource access** | ✅ resources/list, resources/read | ❌ client-side | |
| **OAuth flow** | ✅ MCP OAuth for server auth | ❌ | |
| **Federation** | ❌ | ✅ Node-to-node MCP federation | FF advantage |

---

## 7. CONFIGURATION & SETTINGS

| Feature | CC | FF | Gap |
|---------|----|----|-----|
| **Config file** | ✅ settings.json + .claude/settings.json | ✅ fleet.toml | Different format, similar concept |
| **Project-level config** | ✅ .claude/settings.json per project | ❌ | Need project-level overrides |
| **CLAUDE.md memory files** | ✅ Hierarchical discovery | ❌ | Need equivalent memory file system |
| **Hot-reload** | ✅ File watcher | ✅ File watcher for fleet.toml | Parity ✅ |
| **Environment overrides** | ✅ CLAUDE_* env vars | ✅ FORGEFLEET_* env vars | Parity ✅ |
| **Feature flags** | ✅ 90+ flags via GrowthBook | ❌ | Need feature flag system |
| **Model selection** | ✅ Per-command model override | 🟡 Per-session model | Need per-command model switching |
| **Theme/styling** | ✅ Multiple themes | ❌ | Low priority |
| **Keybindings** | ✅ Customizable (~/.claude/keybindings.json) | ❌ | Low priority |
| **Output format** | ✅ Text/JSON/StreamJSON | ❌ | Need for CLI automation |
| **Skills config** | ✅ .claude/commands/, skill marketplace | 🟡 ff-skills has OpenClaw loader | Need skill config integration |
| **Hooks config** | ✅ settings.json hooks section | ❌ | Need hook configuration |

---

## 8. SESSION & HISTORY

| Feature | CC | FF | Gap |
|---------|----|----|-----|
| **Save/resume sessions** | ✅ JSONL in ~/.claude/sessions/ | ❌ | Need session persistence |
| **Session branching** | ✅ Fork at any message | ❌ | |
| **History search** | ✅ Full-text search across sessions | ❌ | |
| **Session export** | ✅ JSON/Markdown export | ❌ | |
| **Context compaction** | ✅ Auto-compact at 90% | ❌ | **HIGH PRIORITY** |
| **Message rewind** | ✅ /rewind to undo turns | ❌ | |

---

## 9. UI / TUI

| Feature | CC (TypeScript TUI) | FF (React Dashboard) | Gap |
|---------|---------------------|---------------------|-----|
| **CLI/TUI interface** | ✅ ratatui-equivalent (Ink/React terminal) | ❌ No CLI TUI | Need CLI agent interface |
| **Web dashboard** | ❌ | ✅ React 19 dashboard | FF advantage |
| **Chat interface** | ✅ Full TUI with syntax highlighting | ✅ ChatStudio with agent mode | Both have chat |
| **Tool execution cards** | ✅ Collapsible tool results | ✅ Basic tool cards in ChatStudio | FF needs richer rendering |
| **Permission dialogs** | ✅ TUI approval prompts | ❌ | Need approval UI |
| **Session browser** | ✅ List/resume past sessions | ❌ | |
| **Diff viewer** | ✅ Inline git diff | ❌ | |
| **Token/cost display** | ✅ Badge showing usage | ❌ | |
| **Vim mode** | ✅ Full vim keybindings | ❌ | Low priority |
| **Image display** | ✅ Kitty protocol inline images | ❌ | |
| **Syntax highlighting** | ✅ syntect-based | ❌ | |
| **Agent/task views** | ✅ Multi-agent monitoring | 🟡 Basic session list | Need rich agent view |
| **Fleet-specific pages** | ❌ | ✅ 15 pages (Fleet, Topology, Metrics, etc.) | FF advantage |

---

## 10. MULTI-MODEL / ROUTING

| Feature | CC | FF | Gap |
|---------|----|----|-----|
| **Provider abstraction** | ✅ 5 providers (Anthropic, Vertex, Bedrock, Foundry, Claude.ai) | ✅ OpenAI-compat backends | Different approach, both work |
| **Model switching** | ✅ /model command, per-agent override | 🟡 Per-session only | Need dynamic model switching |
| **Tier-based routing** | ❌ Single model | ✅ 4-tier escalation (9B→32B→72B→235B) | FF advantage |
| **Health tracking** | ❌ | ✅ Per-backend health with cooldown | FF advantage |
| **Adaptive routing** | ❌ | ✅ Quality-based backend scoring | FF advantage |
| **Fallback models** | ✅ Fallback on overload | ✅ Tier escalation | Both have fallback |
| **Multi-node distribution** | ❌ Single machine | ✅ Fleet-wide model routing | FF advantage |

---

## 11. HOOKS SYSTEM

| Hook Event | CC | FF | Gap |
|------------|----|----|-----|
| **PreToolUse** | ✅ Block/modify before tool execution | ❌ | Need hook system |
| **PostToolUse** | ✅ React to tool results | ❌ | |
| **PostModelTurn** | ✅ After each LLM response | ❌ | |
| **Stop** | ✅ On conversation end | ❌ | |
| **UserPromptSubmit** | ✅ Before sending user message | ❌ | |
| **Notification** | ✅ Push notification hooks | ❌ | |

---

## 12. MEMORY & CONTEXT

| Feature | CC | FF | Gap |
|---------|----|----|-----|
| **CLAUDE.md hierarchy** | ✅ cwd → parents → home | ❌ | Need FORGEFLEET.md equivalent |
| **Auto memory extraction** | ✅ Extract facts from conversations | 🟡 ff-memory has auto-capture types | Wire to agent loop |
| **Memory consolidation (dream)** | ✅ Background subagent consolidation | ❌ | |
| **RAG pipeline** | ❌ | ✅ ff-memory has RAG | FF advantage |
| **Workspace isolation** | ❌ | ✅ ff-memory per-workspace scoping | FF advantage |

---

## 13. FEATURES FF HAS THAT CC DOESN'T

| Feature | FF | Why It Matters |
|---------|----|----|
| **Multi-node fleet orchestration** | ✅ 6+ nodes with discovery | Distributed compute CC can't do |
| **Tier-based LLM routing** | ✅ 4 tiers with health tracking | Smart model selection |
| **Hardware-aware scheduling** | ✅ GPU/CPU/RAM detection | Right model on right hardware |
| **Leader election** | ✅ With failover | Fleet resilience |
| **State replication** | ✅ Leader→follower WAL sync | Data durability |
| **Self-healing evolution** | ✅ Observe→Analyze→Repair→Verify | Auto-recovery |
| **Canary deployments** | ✅ Phased rollout | Safe updates |
| **Telegram/Discord integration** | ✅ Multi-channel messaging | Chat ops |
| **Voice interface** | ✅ STT/TTS/Twilio | Voice control |
| **Mission Control** | ✅ Sprints, epics, kanban | Project management |
| **Prometheus metrics** | ✅ 12 metric types | Observability |
| **SSH mesh** | ✅ Fleet-wide remote execution | Node management |
| **MCP server** | ✅ 16 tools for external AI | AI integration point |
| **Web dashboard** | ✅ 15 pages, WebSocket real-time | Visual fleet management |

---

## PRIORITY MATRIX: What to Build Next

### P0 — Critical (Blocks basic agent functionality)
1. **AgentTool** — sub-agent spawning (parallel work on fleet nodes)
2. **Auto-compaction** — context window management for long sessions
3. **AskUserQuestionTool** — interactive user input
4. **Session persistence** — save/resume conversations
5. **Streaming responses** — SSE streaming for real-time UI
6. **Parallel tool execution** — concurrent tool calls per turn

### P1 — High (Core Claude Code experience)
7. **TaskCreate/Get/Update/List/Stop/Output tools** — background task management
8. **WebFetchTool** + **WebSearchTool** — web access
9. **EnterPlanMode/ExitPlanMode tools** — planning workflow
10. **Worktree tools** — git isolation
11. **Slash command system** — /help, /clear, /compact, /model, /status, /resume, /rewind
12. **Hook system** — PreToolUse, PostToolUse, Stop hooks
13. **FORGEFLEET.md** — project memory files
14. **MCP Client** — connect to external MCP servers from agent loop
15. **Token/cost tracking** — usage monitoring

### P2 — Medium (Enhanced experience)
16. **NotebookEditTool** — Jupyter support
17. **SkillTool** as agent tool — wire ff-skills to agent loop
18. **Permission levels** on tools — read/write/execute/dangerous
19. **Feature flag system** — runtime feature toggles
20. **CLI TUI** — terminal-based agent interface (ratatui)
21. **Project-level config** — per-project settings overrides
22. **Output format** — Text/JSON/StreamJSON modes
23. **Session branching** — fork conversations
24. **Bash classifier** — smarter command risk assessment
25. **SendMessage/TeamCreate/TeamDelete** — multi-agent coordination

### P3 — Low (Nice to have)
26. **LSP integration** — language server protocol
27. **Vim mode** in CLI
28. **Themes/keybindings**
29. **BuddyTool** — companion pet
30. **ComputerUseTool** — desktop automation
31. **Sandbox mode** — container-based isolation
32. **Secret detection** in output

---

## SUMMARY SCORECARD

| Category | CC Features | FF Has | FF % | Top Gap |
|----------|-------------|--------|------|---------|
| **Tools** | 55 | 6 | 11% | AgentTool, Tasks, Web, Plan |
| **Commands** | 80+ | 0 | 0% | /help, /compact, /model, /status |
| **System Prompt** | 8 features | 1 | 13% | Modular prompt, context injection |
| **Agent Loop** | 14 features | 4 | 29% | Auto-compact, streaming, parallel |
| **Security** | 8 features | 3 | 38% | Per-tool permissions, interactive prompts |
| **MCP** | 6 features | 3 | 50% | MCP client, OAuth |
| **Config** | 12 features | 4 | 33% | Project config, feature flags, hooks |
| **Sessions** | 6 features | 0 | 0% | Save/resume, compaction |
| **UI** | 12 features | 5 | 42% | CLI TUI, diff viewer, token display |
| **Routing** | 6 features | 5 | 83% | Dynamic model switching |
| **Hooks** | 6 events | 0 | 0% | Entire hook system |
| **Memory** | 5 features | 3 | 60% | FORGEFLEET.md, auto-extract |
| **FF-only features** | 0 | 14 | ∞ | Fleet, replication, evolution, voice |

**Overall: ForgeFleet has ~30% feature parity with Claude Code's agent capabilities, but has 14 features CC doesn't have (fleet orchestration, multi-node, self-healing, etc.)**
