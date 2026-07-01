import { Card } from './ui/card'

export function PageSkeleton() {
  return (
    <section className="min-h-full space-y-5 bg-background p-4 text-foreground md:p-6">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="space-y-2">
          <div className="h-8 w-56 animate-pulse rounded-lg bg-elevated" />
          <div className="h-4 w-96 animate-pulse rounded-lg bg-elevated" />
        </div>
        <div className="h-9 w-24 animate-pulse rounded-lg bg-elevated" />
      </div>
      <div className="grid gap-3 sm:grid-cols-3">
        <Card className="h-24 animate-pulse bg-panel" />
        <Card className="h-24 animate-pulse bg-panel" />
        <Card className="h-24 animate-pulse bg-panel" />
      </div>
      <Card className="h-96 animate-pulse bg-panel" />
    </section>
  )
}
