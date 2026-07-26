import { type ReactNode, useState } from 'react'
import { FolderKanban } from 'lucide-react'
import { cn } from '../lib/utils'

type ProjectCardProject = {
  id: string
  name: string
  logo_url?: string | null
}

type ProjectCardProps = {
  project: ProjectCardProject
  selected: boolean
  onSelect: () => void
  subtitle: string
  status: ReactNode
  children?: ReactNode
}

export function ProjectCard({
  project,
  selected,
  onSelect,
  subtitle,
  status,
  children,
}: ProjectCardProps) {
  const [failedLogoUrl, setFailedLogoUrl] = useState<string | null>(null)
  const showLogo = Boolean(project.logo_url) && failedLogoUrl !== project.logo_url

  return (
    <button
      onClick={onSelect}
      className={cn(
        'w-full rounded-xl border p-4 text-left transition',
        selected
          ? 'border-primary bg-primary-subtle'
          : 'border-border bg-panel hover:border-border-subtle hover:bg-elevated',
      )}
      type="button"
    >
      <div className="flex items-start justify-between gap-2">
        <div className="flex min-w-0 items-center gap-3">
          <div className="flex h-10 w-10 shrink-0 items-center justify-center overflow-hidden rounded-lg border border-border bg-surface">
            {showLogo ? (
              <img
                src={project.logo_url!}
                alt=""
                className="h-full w-full object-contain"
                crossOrigin="anonymous"
                referrerPolicy="no-referrer"
                onError={() => setFailedLogoUrl(project.logo_url ?? null)}
              />
            ) : (
              <FolderKanban aria-hidden="true" className="h-5 w-5 text-dim" />
            )}
          </div>
          <div className="min-w-0">
            <p className="truncate text-sm font-semibold text-foreground">{project.name}</p>
            <p className="mt-1 truncate text-xs text-dim">{subtitle}</p>
          </div>
        </div>
        {status}
      </div>

      {children}
    </button>
  )
}
