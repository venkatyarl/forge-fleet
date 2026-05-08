import { useNavigate } from 'react-router-dom'

export function NotFound() {
  const navigate = useNavigate()

  return (
    <div className="flex h-full flex-col items-center justify-center text-center">
      <div className="mb-4 text-6xl font-bold text-zinc-700">404</div>
      <h1 className="mb-2 text-xl font-semibold text-zinc-200">Page not found</h1>
      <p className="mb-6 max-w-md text-sm text-zinc-400">
        The page you are looking for does not exist or has been moved.
      </p>
      <button
        onClick={() => navigate('/')}
        className="rounded-md bg-indigo-600 px-4 py-2 text-sm font-medium text-white hover:bg-indigo-500"
      >
        Back to Mission Control
      </button>
    </div>
  )
}
