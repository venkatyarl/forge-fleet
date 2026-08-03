'use client'

import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  createContext,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from 'react'
import { useKeyboardShortcuts } from '@/hooks/useKeyboardShortcuts'

interface ProvidersProps {
  children: ReactNode
}

// Dark mode lives in the providers (the root layout is a server component and
// cannot hold state). Same contract as the dashboard Shell: localStorage
// 'ff_dark_mode', default true, toggles the 'dark' class on <html>.
const DarkModeContext = createContext<{
  darkMode: boolean
  toggleDarkMode: () => void
}>({ darkMode: true, toggleDarkMode: () => {} })

export function useDarkMode() {
  return useContext(DarkModeContext)
}

export function Providers({ children }: ProvidersProps) {
  const [queryClient] = useState(
    () =>
      new QueryClient({
        defaultOptions: {
          queries: {
            staleTime: 10_000,
            refetchOnWindowFocus: false,
            retry: 1,
          },
        },
      })
  )
  // SSR-safe initial value; the real preference is read post-mount.
  const [darkMode, setDarkMode] = useState(true)
  const [loaded, setLoaded] = useState(false)

  useKeyboardShortcuts()

  useEffect(() => {
    const cached = localStorage.getItem('ff_dark_mode')
    setDarkMode(cached ? cached === 'true' : true)
    setLoaded(true)
  }, [])

  useEffect(() => {
    document.documentElement.classList.toggle('dark', darkMode)
    if (loaded) localStorage.setItem('ff_dark_mode', String(darkMode))
  }, [darkMode, loaded])

  return (
    <QueryClientProvider client={queryClient}>
      <DarkModeContext.Provider
        value={{ darkMode, toggleDarkMode: () => setDarkMode((prev) => !prev) }}
      >
        {children}
      </DarkModeContext.Provider>
    </QueryClientProvider>
  )
}
