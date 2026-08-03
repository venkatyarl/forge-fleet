import type { Metadata } from 'next'
import './globals.css'
import { Providers } from './providers'

export const metadata: Metadata = {
  title: 'ForgeFleet',
  description:
    'ForgeFleet command mesh dashboard for live fleet telemetry, mission control, and LLM routing insights.',
}

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode
}>) {
  return (
    // 'dark' is the default theme; Providers may toggle the class client-side
    // from localStorage, hence suppressHydrationWarning.
    <html lang="en" className="dark" suppressHydrationWarning>
      <body className="min-h-screen bg-background text-foreground">
        <Providers>{children}</Providers>
      </body>
    </html>
  )
}
