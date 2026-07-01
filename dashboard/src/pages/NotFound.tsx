import { useNavigate } from 'react-router-dom'
import { Button } from '../components/ui/button'
import { Card, CardDescription, CardHeader, CardTitle } from '../components/ui/card'

export function NotFound() {
  const navigate = useNavigate()

  return (
    <div className="flex h-full flex-col items-center justify-center bg-background p-6 text-center text-foreground">
      <Card className="max-w-md bg-panel px-8 py-10">
        <CardHeader className="mb-4 flex-col items-center gap-3">
          <div className="text-6xl font-bold text-dim">404</div>
          <CardTitle className="text-xl">Page not found</CardTitle>
          <CardDescription>
            The page you are looking for does not exist or has been moved.
          </CardDescription>
        </CardHeader>
        <Button onClick={() => navigate('/')}>
          Back to Mission Control
        </Button>
      </Card>
    </div>
  )
}
