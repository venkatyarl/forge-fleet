import BrainPage from '../BrainPage'

// Static export: prerender a placeholder; the client router handles real
// threadSlug values at runtime.
export function generateStaticParams() {
  return [{ threadSlug: 'placeholder' }]
}

export default function BrainThreadPage() {
  return <BrainPage />
}
