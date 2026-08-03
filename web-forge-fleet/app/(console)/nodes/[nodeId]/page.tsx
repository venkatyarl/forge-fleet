import NodeDetailClient from './NodeDetailClient'

// Static export: prerender a placeholder; the client router handles real
// nodeId values at runtime.
export function generateStaticParams() {
  return [{ nodeId: 'placeholder' }]
}

export default function NodeDetailPage() {
  return <NodeDetailClient />
}
