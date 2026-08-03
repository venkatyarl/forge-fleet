import Link from 'next/link'
import { ArrowLeft } from 'lucide-react'
import { cn } from '@/lib/utils'

/**
 * Minimal top bar for standalone pages (workstreams, playground) that live
 * outside the (console) shell: back link to the landing page + page title.
 * Server-component safe — no hooks.
 */
export function StandaloneTopBar({
  title,
  subtitle,
  children,
  className,
}: {
  title: string
  subtitle?: string
  children?: React.ReactNode
  className?: string
}) {
  return (
    <header
      className={cn(
        'sticky top-0 z-20 border-b border-border bg-background/80 backdrop-blur-sm',
        className
      )}
    >
      <div className="mx-auto flex h-14 max-w-6xl items-center gap-3 px-4 sm:px-6">
        <Link
          href="/"
          className="inline-flex h-8 w-8 items-center justify-center rounded-lg border border-border bg-panel text-muted transition-colors hover:bg-elevated hover:text-foreground"
          aria-label="Back to home"
        >
          <ArrowLeft className="h-4 w-4" />
        </Link>
        <div className="min-w-0">
          <h1 className="truncate text-sm font-semibold text-foreground">{title}</h1>
          {subtitle ? <p className="truncate text-2xs text-dim">{subtitle}</p> : null}
        </div>
        <div className="ml-auto flex items-center gap-2">{children}</div>
      </div>
    </header>
  )
}
