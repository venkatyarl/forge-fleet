import { Component, type ReactNode } from 'react'
import { Card, CardDescription, CardHeader, CardTitle } from './ui/card'
import { Button } from './ui/button'

interface Props {
  children: ReactNode
  fallback?: ReactNode
}

interface State {
  error: Error | null
}

export class ErrorBoundary extends Component<Props, State> {
  constructor(props: Props) {
    super(props)
    this.state = { error: null }
  }

  static getDerivedStateFromError(error: Error): State {
    return { error }
  }

  render() {
    if (this.state.error) {
      if (this.props.fallback) {
        return this.props.fallback
      }
      return (
        <div className="flex h-full flex-col items-center justify-center bg-background p-6 text-center text-foreground">
          <Card className="max-w-md bg-panel px-8 py-10">
            <CardHeader className="mb-4 flex-col items-center gap-3">
              <CardTitle className="text-xl">Something went wrong</CardTitle>
              <CardDescription className="mt-2">
                {this.state.error.message || 'An unexpected error occurred.'}
              </CardDescription>
            </CardHeader>
            <Button onClick={() => window.location.reload()}>Reload page</Button>
          </Card>
        </div>
      )
    }
    return this.props.children
  }
}
