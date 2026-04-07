# CC_Sources Folder Analysis — Features for ForgeFleet

## Tier 1: Critical (implement first)

### awesome-claude-code-subagents-main
- **130+ pre-built agent role templates** with model routing and tool assignment
- **→ Built:** agent_roles.rs with 16 roles. Expand to 50+ from this source.

### claude-code-router-main
- **Request/response transformer pipeline** for multi-provider compatibility
- **Config preset save/load system**
- **Router dashboard UI** component
- **→ Gap:** ForgeFleet's routing is hardcoded. Need configurable transformer middleware.

### claude_agent_teams_ui-main
- **Real-time team kanban board** with agent assignments
- **Agent-to-agent messaging mailbox system**
- **Session analysis with 6-category token tracking**
- **Hunk-level code review** with accept/reject per change
- **Task dependency engine** with blockedBy relationships
- **→ Gap:** Dashboard needs team coordination UI. Migrate from MC legacy review system.

### claude-notifications-go-main
- **Desktop notifications** for task completion
- **Webhook integrations** (Slack, Discord, Telegram)
- **Click-to-focus terminal switching**
- **Notification filtering and suppression**
- **→ Built:** notifications.rs with desktop + webhook + Slack + Discord + Telegram

## Tier 2: Important

### claude-code-templates-main
- **Template registry** with 100+ agents, commands, MCPs, hooks
- **NPX-style installer CLI** for browsing and installing
- **Web dashboard** for template discovery
- **→ Gap:** Need a template registry system in ForgeFleet

### skills-main
- **SKILL.md manifest standard** with YAML frontmatter
- **Document processing skills** (PDF, Excel, Word, PowerPoint)
- **Skill composition and chaining**
- **→ Gap:** Need document processing skills for non-code files

### claude-code-action-main
- **GitHub Actions integration** — trigger ForgeFleet from CI/CD
- **Automated PR review** with hunk-level feedback
- **Issue triage and labeling**
- **→ Gap:** No GitHub Actions integration yet

### claude-code-system-prompts-main
- **System prompt fragment library** for different modes
- **Permission classifier prompts**
- **Safety overlay / guardrail injections**
- **→ Partially built:** system_prompt.rs has modular builder. Need mode variants.

## Tier 3: Nice to have

### claude-multimodel-main
- Feature flag build system (54 flags)
- Multi-provider abstraction (Bedrock, Vertex, Foundry)

### everything-claude-code-main
- Hook adapter pattern for normalized execution
- Performance tracking in hooks

### claude-cookbooks-main
- API pattern cookbook with runnable examples
- RAG implementation patterns

### start-claude-code-main
- One-command launcher pattern
- OAuth token reuse from existing installations
