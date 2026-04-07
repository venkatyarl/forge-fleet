export async function parseJsonSafe<T>(res: Response): Promise<T | null> {
  const text = await res.text()
  if (!text) return null
  try {
    return JSON.parse(text) as T
  } catch {
    return null
  }
}

const NO_CACHE_HEADERS: Record<string, string> = {
  'Cache-Control': 'no-cache',
  Pragma: 'no-cache',
}

export async function getJson<T>(path: string): Promise<T> {
  const res = await fetch(path, {
    cache: 'no-store',
    headers: NO_CACHE_HEADERS,
  })

  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}`)
  }

  const data = await parseJsonSafe<T>(res)
  if (data === null) {
    throw new Error(`Invalid JSON returned from ${path}`)
  }
  return data
}

export async function getText(path: string): Promise<string> {
  const res = await fetch(path, {
    cache: 'no-store',
    headers: NO_CACHE_HEADERS,
  })

  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}`)
  }
  return await res.text()
}

export async function postJson<T>(path: string, payload: unknown): Promise<T | null> {
  const res = await fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  })

  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}`)
  }

  return await parseJsonSafe<T>(res)
}

export async function patchJson<T>(path: string, payload: unknown): Promise<T | null> {
  const res = await fetch(path, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  })

  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}`)
  }

  return await parseJsonSafe<T>(res)
}

export async function deleteJson<T>(path: string, payload?: unknown): Promise<T | null> {
  const hasBody = payload !== undefined
  const res = await fetch(path, {
    method: 'DELETE',
    headers: hasBody ? { 'Content-Type': 'application/json' } : undefined,
    body: hasBody ? JSON.stringify(payload) : undefined,
  })

  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}`)
  }

  return await parseJsonSafe<T>(res)
}

