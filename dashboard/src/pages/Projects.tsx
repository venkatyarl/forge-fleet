import { type FormEvent, useCallback, useEffect, useMemo, useState } from 'react'
import { getJson, patchJson, postJson } from '../lib/api'

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

function asStatus(value: string): PortfolioStatus {
  return PORTFOLIO_STATUSES.includes(value as PortfolioStatus) ? (value as PortfolioStatus) : 'active'
}

function humanize(value: string): string {
  return value.replaceAll('_', ' ')
}

function statusClass(status: string): string {
  switch (status) {
    case 'active':
      return 'bg-emerald-500/15 text-emerald-300 border-emerald-500/30'
    case 'at_risk':
      return 'bg-orange-500/15 text-orange-300 border-orange-500/30'
    case 'paused':
      return 'bg-amber-500/15 text-amber-300 border-amber-500/30'
    case 'completed':
      return 'bg-sky-500/15 text-sky-300 border-sky-500/30'
    case 'archived':
      return 'bg-slate-500/15 text-slate-300 border-slate-500/30'
    default:
      return 'bg-purple-500/15 text-purple-300 border-purple-500/30'
  }
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
    <section className="space-y-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Projects</h1>
          <p className="mt-1 text-sm text-slate-400">
            Mission Control portfolio screen with companies, projects, repos, and environments.
          </p>
        </div>
        <button
          onClick={() => void loadCore()}
          className="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500"
        >
          ↻ Refresh
        </button>
      </div>

      {error ? (
        <div className="rounded-xl border border-rose-500/30 bg-rose-500/10 px-4 py-3 text-sm text-rose-200">
          {error}
        </div>
      ) : null}
      {notice ? (
        <div className="rounded-xl border border-emerald-500/30 bg-emerald-500/10 px-4 py-3 text-sm text-emerald-200">
          {notice}
        </div>
      ) : null}

      <div className="grid gap-3 sm:grid-cols-3">
        <MetricCard
          label="Companies"
          value={summary?.total_companies ?? companies.length}
          color="text-sky-300"
        />
        <MetricCard
          label="Projects"
          value={summary?.total_projects ?? projects.length}
          color="text-purple-300"
        />
        <MetricCard
          label="Active Projects"
          value={summary?.active_projects ?? 0}
          color="text-emerald-300"
        />
      </div>

      <div className="grid gap-4 xl:grid-cols-2">
        <form onSubmit={createCompany} className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
          <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">
            Create Company
          </h2>
          <div className="grid gap-2 md:grid-cols-2">
            <input
              value={companyName}
              onChange={(event) => setCompanyName(event.target.value)}
              placeholder="Company name"
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
              required
            />
            <input
              value={companyOwner}
              onChange={(event) => setCompanyOwner(event.target.value)}
              placeholder="Owner"
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
            />
            <input
              value={companyUnit}
              onChange={(event) => setCompanyUnit(event.target.value)}
              placeholder="Business unit"
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
            />
            <select
              value={companyStatus}
              onChange={(event) => setCompanyStatus(event.target.value as PortfolioStatus)}
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
            >
              {PORTFOLIO_STATUSES.map((status) => (
                <option key={status} value={status}>
                  {humanize(status)}
                </option>
              ))}
            </select>
          </div>
          <button
            type="submit"
            disabled={creatingCompany}
            className="mt-3 rounded-md border border-sky-500/40 bg-sky-500/10 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/20 disabled:opacity-60"
          >
            {creatingCompany ? 'Creating…' : 'Create Company'}
          </button>
        </form>

        <form onSubmit={createProject} className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
          <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-slate-300">
            Create Project
          </h2>
          <div className="grid gap-2 md:grid-cols-2">
            <input
              value={projectName}
              onChange={(event) => setProjectName(event.target.value)}
              placeholder="Project name"
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
              required
            />
            <select
              value={projectCompanyId}
              onChange={(event) => setProjectCompanyId(event.target.value)}
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
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
              value={projectOwner}
              onChange={(event) => setProjectOwner(event.target.value)}
              placeholder="Owner"
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
            />
            <select
              value={projectStatus}
              onChange={(event) => setProjectStatus(event.target.value as PortfolioStatus)}
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
            >
              {PORTFOLIO_STATUSES.map((status) => (
                <option key={status} value={status}>
                  {humanize(status)}
                </option>
              ))}
            </select>
            <select
              value={projectStage}
              onChange={(event) => setProjectStage(event.target.value as OperatingStage)}
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
            >
              {OPERATING_STAGES.map((stage) => (
                <option key={stage} value={stage}>
                  {humanize(stage)}
                </option>
              ))}
            </select>
            <input
              value={projectDescription}
              onChange={(event) => setProjectDescription(event.target.value)}
              placeholder="Description"
              className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
            />
          </div>
          <button
            type="submit"
            disabled={creatingProject || companies.length === 0}
            className="mt-3 rounded-md border border-sky-500/40 bg-sky-500/10 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/20 disabled:opacity-60"
          >
            {creatingProject ? 'Creating…' : 'Create Project'}
          </button>
          {companies.length === 0 ? (
            <p className="mt-2 text-xs text-amber-300">Create at least one company first.</p>
          ) : null}
        </form>
      </div>

      <div className="grid gap-4 lg:grid-cols-[280px_minmax(0,1fr)]">
        <aside className="rounded-xl border border-slate-800 bg-slate-900/60 p-3">
          <h2 className="mb-2 text-sm font-semibold uppercase tracking-wide text-slate-300">
            Project Inventory ({projects.length})
          </h2>
          {loading && projects.length === 0 ? (
            <p className="text-sm text-slate-400">Loading projects…</p>
          ) : projects.length === 0 ? (
            <p className="text-sm text-slate-500">No projects yet.</p>
          ) : (
            <div className="space-y-2">
              {projects.map((project) => {
                const selected = project.id === selectedProjectId
                return (
                  <button
                    key={project.id}
                    onClick={() => setSelectedProjectId(project.id)}
                    className={`w-full rounded-md border px-3 py-2 text-left transition ${
                      selected
                        ? 'border-sky-500/40 bg-sky-500/10'
                        : 'border-slate-800 bg-slate-950/60 hover:border-slate-700'
                    }`}
                    type="button"
                  >
                    <p className="text-sm font-medium text-slate-100">{project.name}</p>
                    <div className="mt-1 flex items-center gap-1.5 text-xs text-slate-400">
                      <span className={`rounded-md border px-1.5 py-0.5 ${statusClass(String(project.status))}`}>
                        {humanize(String(project.status))}
                      </span>
                      <span>{project.owner || 'unassigned'}</span>
                    </div>
                  </button>
                )
              })}
            </div>
          )}
        </aside>

        <div className="space-y-4">
          {selectedProject ? (
            <>
              <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
                <div className="flex flex-wrap items-start justify-between gap-2">
                  <div>
                    <h3 className="text-lg font-semibold text-slate-100">{selectedProject.name}</h3>
                    <p className="mt-1 text-sm text-slate-400">
                      {selectedProject.description || 'No description provided.'}
                    </p>
                    <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-slate-400">
                      <span>owner: {selectedProject.owner || 'unassigned'}</span>
                      <span>stage: {humanize(String(selectedProject.operating_stage))}</span>
                      <span>
                        company:{' '}
                        {companies.find((company) => company.id === selectedProject.company_id)?.name ??
                          selectedProject.company_id}
                      </span>
                    </div>
                  </div>

                  <div className="flex items-center gap-2">
                    <select
                      value={projectStatusDraft}
                      onChange={(event) => setProjectStatusDraft(event.target.value as PortfolioStatus)}
                      className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
                    >
                      {PORTFOLIO_STATUSES.map((status) => (
                        <option key={status} value={status}>
                          {humanize(status)}
                        </option>
                      ))}
                    </select>
                    <button
                      onClick={() => void saveProjectStatus()}
                      className="rounded-md border border-sky-500/40 bg-sky-500/10 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/20"
                    >
                      Save status
                    </button>
                  </div>
                </div>
              </article>

              <div className="grid gap-4 xl:grid-cols-2">
                <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
                  <h4 className="mb-2 text-sm font-semibold uppercase tracking-wide text-slate-300">
                    Repositories ({repos.length})
                  </h4>
                  {loadingDetails ? (
                    <p className="text-sm text-slate-400">Loading repositories…</p>
                  ) : repos.length === 0 ? (
                    <p className="text-sm text-slate-500">No repositories linked.</p>
                  ) : (
                    <ul className="space-y-2 text-sm">
                      {repos.map((repo) => (
                        <li key={repo.id} className="rounded-md border border-slate-800 bg-slate-950/60 p-2">
                          <a
                            href={repo.repository_url}
                            target="_blank"
                            rel="noreferrer"
                            className="text-sky-300 hover:text-sky-200"
                          >
                            {repo.repository_url}
                          </a>
                          <div className="mt-1 text-xs text-slate-400">
                            {repo.provider} · {repo.default_branch} · {humanize(String(repo.status))}
                          </div>
                        </li>
                      ))}
                    </ul>
                  )}

                  <form onSubmit={addRepo} className="mt-3 space-y-2">
                    <input
                      value={repoUrl}
                      onChange={(event) => setRepoUrl(event.target.value)}
                      placeholder="https://github.com/org/repo"
                      className="w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
                      required
                    />
                    <div className="grid gap-2 md:grid-cols-2">
                      <input
                        value={repoProvider}
                        onChange={(event) => setRepoProvider(event.target.value)}
                        placeholder="provider"
                        className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
                      />
                      <input
                        value={repoBranch}
                        onChange={(event) => setRepoBranch(event.target.value)}
                        placeholder="default branch"
                        className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
                      />
                    </div>
                    <button
                      type="submit"
                      disabled={addingRepo}
                      className="rounded-md border border-sky-500/40 bg-sky-500/10 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/20 disabled:opacity-60"
                    >
                      {addingRepo ? 'Adding…' : 'Add repo'}
                    </button>
                  </form>
                </article>

                <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
                  <h4 className="mb-2 text-sm font-semibold uppercase tracking-wide text-slate-300">
                    Environments ({environments.length})
                  </h4>
                  {loadingDetails ? (
                    <p className="text-sm text-slate-400">Loading environments…</p>
                  ) : environments.length === 0 ? (
                    <p className="text-sm text-slate-500">No environments defined.</p>
                  ) : (
                    <ul className="space-y-2 text-sm">
                      {environments.map((environment) => (
                        <li
                          key={environment.id}
                          className="rounded-md border border-slate-800 bg-slate-950/60 p-2"
                        >
                          <p className="font-medium text-slate-100">{environment.name}</p>
                          <div className="mt-1 text-xs text-slate-400">
                            {environment.environment_type} · {environment.owner || 'unassigned'} ·{' '}
                            {humanize(String(environment.status))}
                          </div>
                          {environment.endpoint_url ? (
                            <a
                              href={environment.endpoint_url}
                              target="_blank"
                              rel="noreferrer"
                              className="mt-1 inline-block text-xs text-sky-300 hover:text-sky-200"
                            >
                              {environment.endpoint_url}
                            </a>
                          ) : null}
                        </li>
                      ))}
                    </ul>
                  )}

                  <form onSubmit={addEnvironment} className="mt-3 space-y-2">
                    <div className="grid gap-2 md:grid-cols-2">
                      <input
                        value={envName}
                        onChange={(event) => setEnvName(event.target.value)}
                        placeholder="Environment name"
                        className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
                        required
                      />
                      <input
                        value={envType}
                        onChange={(event) => setEnvType(event.target.value)}
                        placeholder="runtime / staging / prod"
                        className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
                      />
                      <input
                        value={envOwner}
                        onChange={(event) => setEnvOwner(event.target.value)}
                        placeholder="Owner"
                        className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
                      />
                      <input
                        value={envEndpoint}
                        onChange={(event) => setEnvEndpoint(event.target.value)}
                        placeholder="Endpoint URL"
                        className="rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 outline-none focus:border-sky-500"
                      />
                    </div>
                    <button
                      type="submit"
                      disabled={addingEnvironment}
                      className="rounded-md border border-sky-500/40 bg-sky-500/10 px-3 py-2 text-sm text-sky-300 hover:bg-sky-500/20 disabled:opacity-60"
                    >
                      {addingEnvironment ? 'Adding…' : 'Add environment'}
                    </button>
                  </form>
                </article>
              </div>
            </>
          ) : (
            <div className="rounded-xl border border-slate-800 bg-slate-900/60 px-4 py-6 text-sm text-slate-400">
              Select a project to view repositories and environments.
            </div>
          )}
        </div>
      </div>

      <div className="grid gap-4 lg:grid-cols-2">
        <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
          <h2 className="mb-2 text-sm font-semibold uppercase tracking-wide text-slate-300">
            Status Breakdown
          </h2>
          {summary?.projects_by_status && Object.keys(summary.projects_by_status).length > 0 ? (
            <div className="space-y-2 text-sm">
              {Object.entries(summary.projects_by_status)
                .sort(([, a], [, b]) => b - a)
                .map(([status, count]) => (
                  <div key={status} className="flex items-center justify-between rounded-md bg-slate-950/60 px-3 py-2">
                    <span className={`rounded-md border px-2 py-0.5 text-xs ${statusClass(status)}`}>
                      {humanize(status)}
                    </span>
                    <span className="font-medium text-slate-200">{count}</span>
                  </div>
                ))}
            </div>
          ) : (
            <p className="text-sm text-slate-500">No status summary yet.</p>
          )}
        </article>

        <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
          <h2 className="mb-2 text-sm font-semibold uppercase tracking-wide text-slate-300">
            Operating Stage Breakdown
          </h2>
          {summary?.projects_by_operating_stage &&
          Object.keys(summary.projects_by_operating_stage).length > 0 ? (
            <div className="space-y-2 text-sm">
              {Object.entries(summary.projects_by_operating_stage)
                .sort(([, a], [, b]) => b - a)
                .map(([stage, count]) => (
                  <div key={stage} className="flex items-center justify-between rounded-md bg-slate-950/60 px-3 py-2">
                    <span className="text-slate-300">{humanize(stage)}</span>
                    <span className="font-medium text-slate-200">{count}</span>
                  </div>
                ))}
            </div>
          ) : (
            <p className="text-sm text-slate-500">No stage summary yet.</p>
          )}
        </article>
      </div>
    </section>
  )
}

function MetricCard({ label, value, color }: { label: string; value: number; color: string }) {
  return (
    <div className="rounded-xl border border-slate-800 bg-slate-900/50 px-4 py-3">
      <dt className="text-xs uppercase tracking-wider text-slate-500">{label}</dt>
      <dd className={`text-2xl font-bold ${color}`}>{value}</dd>
    </div>
  )
}
