import { cva, type VariantProps } from 'class-variance-authority'
import { cn } from '../../lib/utils'

const badgeVariants = cva(
  'inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-2xs font-medium ring-1 ring-inset transition-colors',
  {
    variants: {
      variant: {
        default: 'bg-primary-subtle text-primary ring-primary/30',
        ok: 'bg-emerald-500/10 text-status-ok ring-emerald-500/30',
        warn: 'bg-amber-500/10 text-status-warn ring-amber-500/30',
        crit: 'bg-rose-500/10 text-status-crit ring-rose-500/30',
        info: 'bg-sky-500/10 text-status-info ring-sky-500/30',
        neutral: 'bg-panel text-muted ring-border',
      },
    },
    defaultVariants: {
      variant: 'neutral',
    },
  }
)

export interface BadgeProps
  extends React.HTMLAttributes<HTMLSpanElement>,
    VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, ...props }: BadgeProps) {
  return <span className={cn(badgeVariants({ variant }), className)} {...props} />
}
