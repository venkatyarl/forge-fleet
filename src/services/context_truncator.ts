/** Rough chars-per-token heuristic used to convert a token budget into a char budget. */
export const CHARS_PER_TOKEN = 4;

/** Default context budget (~8k tokens) applied when the query sets no maxTokens. */
export const DEFAULT_MAX_CONTEXT_CHARS = 32768;

export function maxCharsForTokens(maxTokens?: number): number {
  if (maxTokens === undefined || !Number.isFinite(maxTokens) || maxTokens <= 0) {
    return DEFAULT_MAX_CONTEXT_CHARS;
  }
  return Math.floor(maxTokens * CHARS_PER_TOKEN);
}

/**
 * Trim an ordered list of context chunks to fit a character budget.
 * Chunks are kept whole while they fit; the chunk that crosses the budget is
 * sliced to the remaining room and everything after it is dropped.
 */
export function truncateContext(
  chunks: string[],
  maxChars: number = DEFAULT_MAX_CONTEXT_CHARS,
): string[] {
  const out: string[] = [];
  let used = 0;
  for (const chunk of chunks) {
    const remaining = maxChars - used;
    if (remaining <= 0) {
      break;
    }
    if (chunk.length <= remaining) {
      out.push(chunk);
      used += chunk.length;
    } else {
      out.push(chunk.slice(0, remaining));
      break;
    }
  }
  return out;
}
