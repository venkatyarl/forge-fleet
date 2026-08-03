import type { Metadata } from 'next'

export const metadata: Metadata = {
  title: 'Workstreams · ForgeFleet',
  description: 'Live session-of-record: which CLI session owns which lane across the fleet.',
}

export default function WorkstreamsLayout({ children }: { children: React.ReactNode }) {
  return children
}
