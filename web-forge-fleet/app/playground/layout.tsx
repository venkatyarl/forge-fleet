import type { Metadata } from 'next'

export const metadata: Metadata = {
  title: 'Playground · ForgeFleet',
  description: 'Streaming chat playground for every model serving on the fleet.',
}

export default function PlaygroundLayout({ children }: { children: React.ReactNode }) {
  return children
}
