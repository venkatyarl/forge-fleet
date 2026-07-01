import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter } from 'react-router-dom'
import { Providers } from './app/providers'
import { ErrorBoundary } from './components/ErrorBoundary'
import App from './App'
import './index.css'

// Top-level ErrorBoundary: the app Shell (Header/Sidebar/CommandPalette/hooks)
// renders OUTSIDE the per-page ErrorBoundary, so a throw there used to unmount
// the whole tree → silent black screen. This outermost boundary catches any
// render throw (chrome, providers, router) and shows the error message + a
// reload button instead of blanking.
createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <ErrorBoundary>
      <BrowserRouter>
        <Providers>
          <App />
        </Providers>
      </BrowserRouter>
    </ErrorBoundary>
  </StrictMode>,
)
