import { ContextQueryParams, validateContextQueryParams } from '../schemas/context_query';
import { maxCharsForTokens, truncateContext } from './context_truncator';

export type ContextQuery = ContextQueryParams;

/**
 * Result shape returned by standard PostgreSQL clients (pg's Pool/Client):
 * matched rows live under `rows`, never as a bare array.
 */
export interface DbQueryResult {
  rows: Array<Record<string, unknown>>;
}

export interface DbClient {
  query(text: string, values?: unknown[]): Promise<DbQueryResult>;
}

/** Cap on rows fetched per context query, independent of the char budget. */
export const MAX_CONTEXT_ROWS = 64;

// Selects only the content column — the embedding/metadata jsonb columns on
// local_context_chunks are by far its heaviest and are never part of agent context.
const FETCH_CONTEXT_SQL = `
SELECT c.content
FROM local_context_chunks c
JOIN local_context_sources s ON s.id = c.source_id
WHERE ($1::uuid IS NULL OR s.id = $1::uuid)
  AND ($2::text IS NULL OR s.title = $2 OR s.uri = $2)
  AND c.content ILIKE '%' || $3 || '%'
ORDER BY c.chunk_index ASC
LIMIT $4
`.trim();

export class DbFetcher {
  constructor(private readonly db: DbClient) {}

  async fetchContext(query: ContextQuery): Promise<string[]> {
    const errors = validateContextQueryParams(query);
    if (errors.length > 0) {
      throw new Error(`invalid context query: ${errors.join('; ')}`);
    }

    const result = await this.db.query(FETCH_CONTEXT_SQL, [
      query.id ?? null,
      query.name ?? null,
      query.query,
      MAX_CONTEXT_ROWS,
    ]);

    const contents = result.rows
      .map((row) => (typeof row.content === 'string' ? row.content : ''))
      .filter((content) => content.length > 0);

    return truncateContext(contents, maxCharsForTokens(query.maxTokens));
  }
}
