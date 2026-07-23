/// Loading skeleton components for shimmer effects.

export function SkeletonCard({ lines = 3 }: { lines?: number }) {
  return (
    <div className="rounded-xl border border-zinc-800 bg-zinc-900 p-4 space-y-3">
      <div className="h-5 w-32 animate-pulse rounded-sm bg-zinc-800" />
      {Array.from({ length: lines }).map((_, i) => (
        <div key={i} className="h-3 animate-pulse rounded-sm bg-zinc-800" style={{ width: `${70 + ((i * 37) % 30)}%` }} />
      ))}
    </div>
  )
}

export function SkeletonStatCard() {
  return (
    <div className="rounded-xl border border-zinc-800 bg-zinc-900 p-4">
      <div className="h-3 w-20 animate-pulse rounded-sm bg-zinc-800 mb-2" />
      <div className="h-8 w-16 animate-pulse rounded-sm bg-zinc-800" />
    </div>
  )
}

export function SkeletonTable({ rows = 5, cols = 4 }: { rows?: number; cols?: number }) {
  return (
    <div className="rounded-xl border border-zinc-800 bg-zinc-900 p-4 space-y-3">
      <div className="flex gap-4">
        {Array.from({ length: cols }).map((_, i) => (
          <div key={i} className="h-3 flex-1 animate-pulse rounded-sm bg-zinc-800" />
        ))}
      </div>
      {Array.from({ length: rows }).map((_, i) => (
        <div key={i} className="flex gap-4">
          {Array.from({ length: cols }).map((_, j) => (
            <div key={j} className="h-3 flex-1 animate-pulse rounded-sm bg-zinc-800" style={{ opacity: 0.5 + ((i * cols + j) % 6) * 0.1 }} />
          ))}
        </div>
      ))}
    </div>
  )
}

export function SkeletonList({ items = 4 }: { items?: number }) {
  return (
    <div className="space-y-2">
      {Array.from({ length: items }).map((_, i) => (
        <div key={i} className="flex items-center gap-3 rounded-lg border border-zinc-800 bg-zinc-900 p-3">
          <div className="h-8 w-8 animate-pulse rounded-full bg-zinc-800" />
          <div className="flex-1 space-y-2">
            <div className="h-3 w-40 animate-pulse rounded-sm bg-zinc-800" />
            <div className="h-2 w-60 animate-pulse rounded-sm bg-zinc-800" />
          </div>
        </div>
      ))}
    </div>
  )
}
