import { type FormEvent, type ReactNode, useCallback, useEffect, useMemo, useState } from 'react'
import { RefreshCw } from 'lucide-react'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { Badge, type BadgeProps } from '../components/ui/badge'
import { Button } from '../components/ui/button'
import { getJson, patchJson, postJson } from '../lib/api'
import { cn } from '../lib/utils'

type PortfolioStatus = 'proposed' | 'active' | 'paused' | 'at_risk' | 'completed' | 'archived'
type OperatingStage =
  | 'discovery'
  | 'validation'
  | 'build'
  | 'launch'
  | 'growth'
  | 'sustain'
  | 'sunset'

type Company = {
  id: string
  name: string
  business_unit?: string | null
  status: PortfolioStatus | string
  owner: string
  operating_stage: OperatingStage | string
  compliance_sensitivity: string
  revenue_model_tags: string[]
}

type Project = {
  id: string
  company_id: string
  name: string
  description: string
  status: PortfolioStatus | string
  owner: string
  operating_stage: OperatingStage | string
  compliance_sensitivity: string
  revenue_model_tags: string[]
  updated_at: string
  repos: ProjectRepoSummary[]
  folders: ProjectFolderSummary[]
}

type ProjectRepoSummary = {
  github_url: string
  name: string
  role: string
  is_primary: boolean
}

type ProjectFolderSummary = {
  path: string
  computer_name: string
  role: string
  is_primary: boolean
}

type ProjectRepo = {
  id: string
  repository_url: string
  provider: string
  default_branch: string
  status: PortfolioStatus | string
}

type ProjectEnvironment = {
  id: string
  name: string
  environment_type: string
  owner: string
  endpoint_url?: string | null
  status: PortfolioStatus | string
}

type PortfolioSummary = {
  total_companies: number
  total_projects: number
  active_projects: number
  projects_by_status: Record<string, number>
  projects_by_operating_stage: Record<string, number>
}

const PORTFOLIO_STATUSES: PortfolioStatus[] = [
  'proposed',
  'active',
  'paused',
  'at_risk',
  'completed',
  'archived',
]

const OPERATING_STAGES: OperatingStage[] = [
  'discovery',
  'validation',
  'build',
  'launch',
  'growth',
  'sustain',
  'sunset',
]

const fieldClass =
  'w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-foreground outline-none transition placeholder:text-dim focus:border-primary disabled:cursor-not-allowed disabled:opacity-60'

const compactFieldClass =
  'rounded-lg border border-border bg-surface px-3 py-2 text-sm text-foreground outline-none transition focus:border-primary'

function asStatus(value: string): PortfolioStatus {
  return PORTFOLIO_STATUSES.includes(value as PortfolioStatus) ? (value as PortfolioStatus) : 'active'
}

function humanize(value: string): string {
  return value.replaceAll('_', ' ')
}

function statusVariant(status: string | null | undefined): BadgeProps['variant'] {
  switch ((status ?? '').toLowerCase()) {
    case 'active':
    case 'completed':
      return 'ok'
    case 'paused':
      return 'warn'
    case 'at_risk':
      return 'crit'
    case 'proposed':
      return 'info'
    case 'archived':
      return 'neutral'
    default:
      return 'default'
  }
}

function statusDotClass(status: string | null | undefined): string {
  switch (statusVariant(status)) {
    case 'ok':
      return 'bg-status-ok'
    case 'warn':
      return 'bg-status-warn'
    case 'crit':
      return 'bg-status-crit'
    case 'info':
      return 'bg-status-info'
    default:
      return 'bg-primary'
  }
}

function StatusBadge({
  status,
  children,
  className,
}: {
  status?: string | null
  children?: ReactNode
  className?: string
}) {
  const label = status ? humanize(status) : 'unknown'
  return (
    <Badge variant={statusVariant(status)} className={className}>
      {children ?? label}
    </Badge>
  )
}

export function Projects() {
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)

  const [summary, setSummary] = useState<PortfolioSummary | null>(null)
  const [companies, setCompanies] = useState<Company[]>([])
  const [projects, setProjects] = useState<Project[]>([])
  const [selectedProjectId, setSelectedProjectId] = useState('')
  const [projectStatusDraft, setProjectStatusDraft] = useState<PortfolioStatus>('active')

  const [repos, setRepos] = useState<ProjectRepo[]>([])
  const [environments, setEnvironments] = useState<ProjectEnvironment[]>([])
  const [loadingDetails, setLoadingDetails] = useState(false)

  const [companyName, setCompanyName] = useState('')
  const [companyOwner, setCompanyOwner] = useState('')
  const [companyStatus, setCompanyStatus] = useState<PortfolioStatus>('active')
  const [companyUnit, setCompanyUnit] = useState('')
  const [creatingCompany, setCreatingCompany] = useState(false)

  const [projectName, setProjectName] = useState('')
  const [projectDescription, setProjectDescription] = useState('')
  const [projectCompanyId, setProjectCompanyId] = useState('')
  const [projectOwner, setProjectOwner] = useState('')
  const [projectStatus, setProjectStatus] = useState<PortfolioStatus>('active')
  const [projectStage, setProjectStage] = useState<OperatingStage>('build')
  const [creatingProject, setCreatingProject] = useState(false)

  const [repoUrl, setRepoUrl] = useState('')
  const [repoProvider, setRepoProvider] = useState('github')
  const [repoBranch, setRepoBranch] = useState('main')
  const [addingRepo, setAddingRepo] = useState(false)

  const [envName, setEnvName] = useState('')
  const [envType, setEnvType] = useState('runtime')
  const [envOwner, setEnvOwner] = useState('')
  const [envEndpoint, setEnvEndpoint] = useState('')
  const [addingEnvironment, setAddingEnvironment] = useState(false)

  const selectedProject = useMemo(
    () => projects.find((project) => project.id === selectedProjectId) ?? null,
    [projects, selectedProjectId],
  )

  const selectedCompanyName = selectedProject
    ? companies.find((company) => company.id === selectedProject.company_id)?.name ?? selectedProject.company_id
    : ''

  const loadCore = useCallback(async () => {
    try {
      setError(null)

      const [summaryPayload, companiesPayload, projectsPayload] = await Promise.all([
        getJson<unknown>('/api/mc/portfolio/summary'),
        getJson<unknown>('/api/mc/companies'),
        getJson<unknown>('/api/mc/projects'),
      ])

      const nextSummary = summaryPayload as PortfolioSummary
      const nextCompanies = Array.isArray(companiesPayload) ? (companiesPayload as Company[]) : []
      const nextProjects = Array.isArray(projectsPayload) ? (projectsPayload as Project[]) : []

      setSummary(nextSummary)
      setCompanies(nextCompanies)
      setProjects(nextProjects)

      if (nextCompanies.length > 0 && !projectCompanyId) {
        setProjectCompanyId(nextCompanies[0].id)
      }

      if (nextProjects.length === 0) {
        setSelectedProjectId('')
      } else if (!nextProjects.some((project) => project.id === selectedProjectId)) {
        setSelectedProjectId(nextProjects[0].id)
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load portfolio data')
    } finally {
      setLoading(false)
    }
  }, [projectCompanyId, selectedProjectId])

  const loadProjectDetails = useCallback(async (projectId: string) => {
    if (!projectId) {
      setRepos([])
      setEnvironments([])
      return
    }

    try {
      setLoadingDetails(true)
      const [reposPayload, envsPayload] = await Promise.all([
        getJson<unknown>(`/api/mc/projects/${projectId}/repos`),
        getJson<unknown>(`/api/mc/projects/${projectId}/environments`),
      ])
      setRepos(Array.isArray(reposPayload) ? (reposPayload as ProjectRepo[]) : [])
      setEnvironments(Array.isArray(envsPayload) ? (envsPayload as ProjectEnvironment[]) : [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load project details')
    } finally {
      setLoadingDetails(false)
    }
  }, [])

  useEffect(() => {
    void loadCore()
    const id = window.setInterval(() => void loadCore(), 30000)
    return () => window.clearInterval(id)
  }, [loadCore])

  useEffect(() => {
    if (selectedProjectId) {
      void loadProjectDetails(selectedProjectId)
    } else {
      setRepos([])
      setEnvironments([])
    }
  }, [selectedProjectId, loadProjectDetails])

  useEffect(() => {
    if (selectedProject) {
      setProjectStatusDraft(asStatus(String(selectedProject.status)))
    }
  }, [selectedProject])

  const createCompany = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!companyName.trim()) return

    try {
      setCreatingCompany(true)
      setError(null)
      setNotice(null)

      await postJson('/api/mc/companies', {
        name: companyName.trim(),
        owner: companyOwner.trim() || undefined,
        status: companyStatus,
        business_unit: companyUnit.trim() || undefined,
      })

      setCompanyName('')
      setCompanyOwner('')
      setCompanyStatus('active')
      setCompanyUnit('')
      setNotice('Company created')
      await loadCore()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create company')
    } finally {
      setCreatingCompany(false)
    }
  }

  const createProject = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!projectName.trim() || !projectCompanyId) return

    try {
      setCreatingProject(true)
      setError(null)
      setNotice(null)

      await postJson('/api/mc/projects', {
        company_id: projectCompanyId,
        name: projectName.trim(),
        description: projectDescription.trim(),
        owner: projectOwner.trim() || undefined,
        status: projectStatus,
        operating_stage: projectStage,
      })

      setProjectName('')
      setProjectDescription('')
      setProjectOwner('')
      setProjectStatus('active')
      setProjectStage('build')
      setNotice('Project created')
      await loadCore()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create project')
    } finally {
      setCreatingProject(false)
    }
  }

  const saveProjectStatus = async () => {
    if (!selectedProject) return

    try {
      setError(null)
      setNotice(null)
      await patchJson(`/api/mc/projects/${selectedProject.id}`, { status: projectStatusDraft })
      setNotice(`Project status updated to ${humanize(projectStatusDraft)}`)
      await loadCore()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update project')
    }
  }

  const addRepo = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!selectedProject || !repoUrl.trim()) return

    try {
      setAddingRepo(true)
      setError(null)
      setNotice(null)

      await postJson(`/api/mc/projects/${selectedProject.id}/repos`, {
        repository_url: repoUrl.trim(),
        provider: repoProvider.trim() || 'github',
        default_branch: repoBranch.trim() || 'main',
      })

      setRepoUrl('')
      setRepoProvider('github')
      setRepoBranch('main')
      setNotice('Repository linked')
      await loadProjectDetails(selectedProject.id)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to add repository')
    } finally {
      setAddingRepo(false)
    }
  }

  const addEnvironment = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (!selectedProject || !envName.trim()) return

    try {
      setAddingEnvironment(true)
      setError(null)
      setNotice(null)

      await postJson(`/api/mc/projects/${selectedProject.id}/environments`, {
        name: envName.trim(),
        environment_type: envType.trim() || 'runtime',
        owner: envOwner.trim() || undefined,
        endpoint_url: envEndpoint.trim() || undefined,
      })

      setEnvName('')
      setEnvType('runtime')
      setEnvOwner('')
      setEnvEndpoint('')
      setNotice('Environment added')
      await loadProjectDetails(selectedProject.id)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to add environment')
    } finally {
      setAddingEnvironment(false)
    }
  }

  return (
    <section className="min-h-full space-y-6 bg-background text-foreground">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-2xl font-bold tracking-tight text-foreground">Projects</h1>
            {loading ? <Badge variant="info">syncing</Badge> : null}
          </div>
          <p className="mt-1 text-sm text-dim">
            Mission Control portfolio screen with companies, projects, repos, and environments.
          </p>
        </div>
        <Button variant="outline" onClick={() => void loadCore()} disabled={loading}>
          <RefreshCw className={cn('h-4 w-4', loading && 'animate-spin')} />
          Refresh
        </Button>
      </div>

      {error ? (
        <Card className="border-status-crit bg-panel">
          <div className="text-sm text-status-crit">{error}</div>
        </Card>
      ) : null}
      {notice ? (
        <Card className="border-status-ok bg-panel">
          <div className="text-sm text-status-ok">{notice}</div>
        </Card>
      ) : null}

      <div className="grid gap-3 sm:grid-cols-3">
        <MetricCard label="Companies" value={summary?.total_companies ?? companies.length} tone="info" />
        <MetricCard label="Projects" value={summary?.total_projects ?? projects.length} tone="primary" />
        <MetricCard label="Active Projects" value={summary?.active_projects ?? 0} tone="ok" />
      </div>

      <div className="grid gap-4 xl:grid-cols-2">
        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Create Company</CardTitle>
              <CardDescription>Add a portfolio owner or operating unit.</CardDescription>
            </div>
          </CardHeader>
          <form onSubmit={createCompany} className="space-y-3">
            <div className="grid gap-2 md:grid-cols-2">
              <input
                aria-label="Company name"
                value={companyName}
                onChange={(event) => setCompanyName(event.target.value)}
                placeholder="Company name"
                className={fieldClass}
                required
              />
              <input
                aria-label="Company owner"
                value={companyOwner}
                onChange={(event) => setCompanyOwner(event.target.value)}
                placeholder="Owner"
                className={fieldClass}
              />
              <input
                aria-label="Business unit"
                value={companyUnit}
                onChange={(event) => setCompanyUnit(event.target.value)}
                placeholder="Business unit"
                className={fieldClass}
              />
              <select
                aria-label="Company status"
                value={companyStatus}
                onChange={(event) => setCompanyStatus(event.target.value as PortfolioStatus)}
                className={fieldClass}
              >
                {PORTFOLIO_STATUSES.map((status) => (
                  <option key={status} value={status}>
                    {humanize(status)}
                  </option>
                ))}
              </select>
            </div>
            <Button type="submit" variant="outline" disabled={creatingCompany}>
              {creatingCompany ? 'Creating...' : 'Create Company'}
            </Button>
          </form>
        </Card>

        <Card className="bg-panel">
          <CardHeader>
            <div>
              <CardTitle>Create Project</CardTitle>
              <CardDescription>Attach work to an existing company.</CardDescription>
            </div>
          </CardHeader>
          <form onSubmit={createProject} className="space-y-3">
            <div className="grid gap-2 md:grid-cols-2">
              <input
                aria-label="Project name"
                value={projectName}
                onChange={(event) => setProjectName(event.target.value)}
                placeholder="Project name"
                className={fieldClass}
                required
              />
              <select
                aria-label="Project company"
                value={projectCompanyId}
                onChange={(event) => setProjectCompanyId(event.target.value)}
                className={fieldClass}
                required
              >
                <option value="" disabled>
                  Select company
                </option>
                {companies.map((company) => (
                  <option key={company.id} value={company.id}>
                    {company.name}
                  </option>
                ))}
              </select>
              <input
                aria-label="Project owner"
                value={projectOwner}
                onChange={(event) => setProjectOwner(event.target.value)}
                placeholder="Owner"
                className={fieldClass}
              />
              <select
                aria-label="Project status"
                value={projectStatus}
                onChange={(event) => setProjectStatus(event.target.value as PortfolioStatus)}
                className={fieldClass}
              >
                {PORTFOLIO_STATUSES.map((status) => (
                  <option key={status} value={status}>
                    {humanize(status)}
                  </option>
                ))}
              </select>
              <select
                aria-label="Project stage"
                value={projectStage}
                onChange={(event) => setProjectStage(event.target.value as OperatingStage)}
                className={fieldClass}
              >
                {OPERATING_STAGES.map((stage) => (
                  <option key={stage} value={stage}>
                    {humanize(stage)}
                  </option>
                ))}
              </select>
              <input
                aria-label="Project description"
                value={projectDescription}
                onChange={(event) => setProjectDescription(event.target.value)}
                placeholder="Description"
                className={fieldClass}
              />
            </div>
            <div className="flex flex-wrap items-center gap-2">
              <Button type="submit" variant="outline" disabled={creatingProject || companies.length === 0}>
                {creatingProject ? 'Creating...' : 'Create Project'}
              </Button>
              {companies.length === 0 ? (
                <span className="text-xs text-status-warn">Create at least one company first.</span>
              ) : null}
            </div>
          </form>
        </Card>
      </div>

      <div className="grid gap-4 lg:grid-cols-[340px_minmax(0,1fr)]">
        <Card className="bg-surface">
          <CardHeader className="items-start gap-3">
            <div>
              <CardTitle>Project Inventory</CardTitle>
              <CardDescription>{projects.length} tracked projects</CardDescription>
            </div>
            <Badge variant="neutral">{projects.length}</Badge>
          </CardHeader>

          {loading && projects.length === 0 ? (
            <EmptyText>Loading projects...</EmptyText>
          ) : projects.length === 0 ? (
            <EmptyText>No projects yet.</EmptyText>
          ) : (
            <div className="space-y-2">
              {projects.map((project) => {
                const selected = project.id === selectedProjectId
                return (
                  <button
                    key={project.id}
                    onClick={() => setSelectedProjectId(project.id)}
                    className={cn(
                      'w-full rounded-xl border p-4 text-left transition',
                      selected
                        ? 'border-primary bg-primary-subtle'
                        : 'border-border bg-panel hover:border-border-subtle hover:bg-elevated',
                    )}
                    type="button"
                  >
                    <div className="flex items-start justify-between gap-2">
                      <div className="min-w-0">
                        <p className="truncate text-sm font-semibold text-foreground">{project.name}</p>
                        <p className="mt-1 truncate text-xs text-dim">
                          {project.owner || 'unassigned'} / {humanize(String(project.operating_stage))}
                        </p>
                      </div>
                      <StatusBadge status={String(project.status)} className="shrink-0" />
                    </div>

                    <InventoryList title="Repos" empty="No repos.">
                      {project.repos.map((repo, index) => (
                        <li key={`${repo.github_url}-${index}`} className="flex min-w-0 flex-wrap items-center gap-1.5">
                          <span className="truncate text-muted">{repo.name}</span>
                          {repo.is_primary ? <Badge variant="default">primary</Badge> : null}
                          <Badge variant="neutral">{repo.role}</Badge>
                        </li>
                      ))}
                    </InventoryList>

                    <InventoryList title="Folders" empty="No folders.">
                      {project.folders.map((folder, index) => (
                        <li key={`${folder.path}-${index}`} className="flex min-w-0 flex-wrap items-center gap-1.5">
                          <span className="truncate text-muted">{folder.path}</span>
                          {folder.is_primary ? <Badge variant="default">primary</Badge> : null}
                          <Badge variant="neutral">{folder.computer_name}</Badge>
                          <Badge variant="neutral">{folder.role}</Badge>
                        </li>
                      ))}
                    </InventoryList>
                  </button>
                )
              })}
            </div>
          )}
        </Card>

        <div className="space-y-4">
          {selectedProject ? (
            <>
              <Card className="bg-panel">
                <div className="flex flex-col gap-4 xl:flex-row xl:items-start xl:justify-between">
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className={cn('h-2.5 w-2.5 rounded-full', statusDotClass(String(selectedProject.status)))} />
                      <CardTitle className="text-lg">{selectedProject.name}</CardTitle>
                      <StatusBadge status={String(selectedProject.status)} />
                    </div>
                    <p className="mt-2 text-sm text-muted">
                      {selectedProject.description || 'No description provided.'}
                    </p>
                    <div className="mt-3 flex flex-wrap items-center gap-2 text-xs text-dim">
                      <Badge variant="neutral">owner: {selectedProject.owner || 'unassigned'}</Badge>
                      <Badge variant="neutral">stage: {humanize(String(selectedProject.operating_stage))}</Badge>
                      <Badge variant="neutral">company: {selectedCompanyName}</Badge>
                    </div>
                  </div>

                  <div className="flex flex-wrap items-center gap-2">
                    <select
                      aria-label="Selected project status"
                      value={projectStatusDraft}
                      onChange={(event) => setProjectStatusDraft(event.target.value as PortfolioStatus)}
                      className={compactFieldClass}
                    >
                      {PORTFOLIO_STATUSES.map((status) => (
                        <option key={status} value={status}>
                          {humanize(status)}
                        </option>
                      ))}
                    </select>
                    <Button variant="outline" onClick={() => void saveProjectStatus()}>
                      Save status
                    </Button>
                  </div>
                </div>
              </Card>

              <div className="grid gap-4 xl:grid-cols-2">
                <Card className="bg-panel">
                  <CardHeader>
                    <div>
                      <CardTitle>Repositories</CardTitle>
                      <CardDescription>{repos.length} linked repositories</CardDescription>
                    </div>
                    <Badge variant="neutral">{repos.length}</Badge>
                  </CardHeader>

                  {loadingDetails ? (
                    <EmptyText>Loading repositories...</EmptyText>
                  ) : repos.length === 0 ? (
                    <EmptyText>No repositories linked.</EmptyText>
                  ) : (
                    <ul className="space-y-2 text-sm">
                      {repos.map((repo) => (
                        <li key={repo.id} className="rounded-lg border border-border bg-surface p-3">
                          <a
                            href={repo.repository_url}
                            target="_blank"
                            rel="noreferrer"
                            className="break-all text-sm font-medium text-primary hover:text-primary-muted"
                          >
                            {repo.repository_url}
                          </a>
                          <div className="mt-2 flex flex-wrap items-center gap-1.5">
                            <Badge variant="neutral">{repo.provider}</Badge>
                            <Badge variant="neutral">{repo.default_branch}</Badge>
                            <StatusBadge status={String(repo.status)} />
                          </div>
                        </li>
                      ))}
                    </ul>
                  )}

                  <form onSubmit={addRepo} className="mt-4 space-y-2 border-t border-border pt-4">
                    <input
                      aria-label="Repository URL"
                      value={repoUrl}
                      onChange={(event) => setRepoUrl(event.target.value)}
                      placeholder="https://github.com/org/repo"
                      className={fieldClass}
                      required
                    />
                    <div className="grid gap-2 md:grid-cols-2">
                      <input
                        aria-label="Repository provider"
                        value={repoProvider}
                        onChange={(event) => setRepoProvider(event.target.value)}
                        placeholder="provider"
                        className={fieldClass}
                      />
                      <input
                        aria-label="Repository default branch"
                        value={repoBranch}
                        onChange={(event) => setRepoBranch(event.target.value)}
                        placeholder="default branch"
                        className={fieldClass}
                      />
                    </div>
                    <Button type="submit" variant="outline" disabled={addingRepo}>
                      {addingRepo ? 'Adding...' : 'Add repo'}
                    </Button>
                  </form>
                </Card>

                <Card className="bg-panel">
                  <CardHeader>
                    <div>
                      <CardTitle>Environments</CardTitle>
                      <CardDescription>{environments.length} runtime definitions</CardDescription>
                    </div>
                    <Badge variant="neutral">{environments.length}</Badge>
                  </CardHeader>

                  {loadingDetails ? (
                    <EmptyText>Loading environments...</EmptyText>
                  ) : environments.length === 0 ? (
                    <EmptyText>No environments defined.</EmptyText>
                  ) : (
                    <ul className="space-y-2 text-sm">
                      {environments.map((environment) => (
                        <li key={environment.id} className="rounded-lg border border-border bg-surface p-3">
                          <div className="flex flex-wrap items-center justify-between gap-2">
                            <p className="font-medium text-foreground">{environment.name}</p>
                            <StatusBadge status={String(environment.status)} />
                          </div>
                          <div className="mt-2 flex flex-wrap items-center gap-1.5">
                            <Badge variant="neutral">{environment.environment_type}</Badge>
                            <Badge variant="neutral">{environment.owner || 'unassigned'}</Badge>
                          </div>
                          {environment.endpoint_url ? (
                            <a
                              href={environment.endpoint_url}
                              target="_blank"
                              rel="noreferrer"
                              className="mt-2 inline-block break-all text-xs text-primary hover:text-primary-muted"
                            >
                              {environment.endpoint_url}
                            </a>
                          ) : null}
                        </li>
                      ))}
                    </ul>
                  )}

                  <form onSubmit={addEnvironment} className="mt-4 space-y-2 border-t border-border pt-4">
                    <div className="grid gap-2 md:grid-cols-2">
                      <input
                        aria-label="Environment name"
                        value={envName}
                        onChange={(event) => setEnvName(event.target.value)}
                        placeholder="Environment name"
                        className={fieldClass}
                        required
                      />
                      <input
                        aria-label="Environment type"
                        value={envType}
                        onChange={(event) => setEnvType(event.target.value)}
                        placeholder="runtime / staging / prod"
                        className={fieldClass}
                      />
                      <input
                        aria-label="Environment owner"
                        value={envOwner}
                        onChange={(event) => setEnvOwner(event.target.value)}
                        placeholder="Owner"
                        className={fieldClass}
                      />
                      <input
                        aria-label="Environment endpoint URL"
                        value={envEndpoint}
                        onChange={(event) => setEnvEndpoint(event.target.value)}
                        placeholder="Endpoint URL"
                        className={fieldClass}
                      />
                    </div>
                    <Button type="submit" variant="outline" disabled={addingEnvironment}>
                      {addingEnvironment ? 'Adding...' : 'Add environment'}
                    </Button>
                  </form>
                </Card>
              </div>
            </>
          ) : (
            <Card className="bg-panel py-8">
              <EmptyText>Select a project to view repositories and environments.</EmptyText>
            </Card>
          )}
        </div>
      </div>

      <div className="grid gap-4 lg:grid-cols-2">
        <BreakdownCard title="Status Breakdown" description="Projects grouped by current state">
          {summary?.projects_by_status && Object.keys(summary.projects_by_status).length > 0 ? (
            <div className="space-y-2 text-sm">
              {Object.entries(summary.projects_by_status)
                .sort(([, a], [, b]) => b - a)
                .map(([status, count]) => (
                  <div
                    key={status}
                    className="flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-2"
                  >
                    <StatusBadge status={status} />
                    <span className="font-medium text-foreground">{count}</span>
                  </div>
                ))}
            </div>
          ) : (
            <EmptyText>No status summary yet.</EmptyText>
          )}
        </BreakdownCard>

        <BreakdownCard title="Operating Stage Breakdown" description="Projects grouped by delivery stage">
          {summary?.projects_by_operating_stage &&
          Object.keys(summary.projects_by_operating_stage).length > 0 ? (
            <div className="space-y-2 text-sm">
              {Object.entries(summary.projects_by_operating_stage)
                .sort(([, a], [, b]) => b - a)
                .map(([stage, count]) => (
                  <div
                    key={stage}
                    className="flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-2"
                  >
                    <span className="text-muted">{humanize(stage)}</span>
                    <span className="font-medium text-foreground">{count}</span>
                  </div>
                ))}
            </div>
          ) : (
            <EmptyText>No stage summary yet.</EmptyText>
          )}
        </BreakdownCard>
      </div>
    </section>
  )
}

function MetricCard({
  label,
  value,
  tone,
}: {
  label: string
  value: number
  tone: 'ok' | 'info' | 'primary'
}) {
  return (
    <Card className="bg-panel px-4 py-3">
      <dt className="text-xs uppercase tracking-wider text-dim">{label}</dt>
      <dd
        className={cn(
          'mt-1 text-2xl font-bold',
          tone === 'ok' && 'text-status-ok',
          tone === 'info' && 'text-status-info',
          tone === 'primary' && 'text-primary',
        )}
      >
        {value}
      </dd>
    </Card>
  )
}

function InventoryList({ title, empty, children }: { title: string; empty: string; children: ReactNode }) {
  const hasChildren = Array.isArray(children) ? children.length > 0 : Boolean(children)

  return (
    <div className="mt-3">
      <h4 className="text-xs font-semibold uppercase tracking-wide text-dim">{title}</h4>
      {hasChildren ? <ul className="mt-1 space-y-1.5 text-sm">{children}</ul> : <p className="mt-1 text-xs text-dim">{empty}</p>}
    </div>
  )
}

function BreakdownCard({ title, description, children }: { title: string; description: string; children: ReactNode }) {
  return (
    <Card className="bg-panel">
      <CardHeader>
        <div>
          <CardTitle>{title}</CardTitle>
          <CardDescription>{description}</CardDescription>
        </div>
      </CardHeader>
      {children}
    </Card>
  )
}

function EmptyText({ children }: { children: ReactNode }) {
  return <p className="text-sm text-dim">{children}</p>
}
