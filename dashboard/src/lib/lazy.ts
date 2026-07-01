import { lazy, type ComponentType } from 'react'

/**
 * Create a React.lazy component from a named export.
 */
export function lazyNamed<T extends object>(
  loader: () => Promise<Record<string, ComponentType<T>>>,
  name: string,
) {
  return lazy(async () => {
    const module = await loader()
    const Component = module[name]
    if (!Component) {
      throw new Error(`lazyNamed: export "${name}" not found`)
    }
    return { default: Component }
  })
}

/**
 * Render a lazy component inside a Suspense-compatible element.
 * Used with React Router routes.
 */
export function lazyPage<T extends object>(
  loader: () => Promise<Record<string, ComponentType<T>>>,
  name: string,
) {
  return lazyNamed(loader, name)
}
