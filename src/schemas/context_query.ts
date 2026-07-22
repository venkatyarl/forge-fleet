export interface ContextQueryParams {
  id?: string;
  name?: string;
  query: string;
  maxTokens?: number;
  verbose?: boolean;
}

export interface ContextChunk {
  source: string;
  content: string;
  score?: number;
  truncated: boolean;
}

export interface ContextResponsePayload {
  query: string;
  chunks: ContextChunk[];
  totalTokens: number;
  truncated: boolean;
}

export function validateContextQueryParams(params: ContextQueryParams): string[] {
  const errors: string[] = [];

  if (!params.query || params.query.trim().length === 0) {
    errors.push('query is required and cannot be empty');
  }

  if (!params.id && !params.name) {
    errors.push('either id or name must be provided');
  }

  if (params.maxTokens !== undefined && (!Number.isFinite(params.maxTokens) || params.maxTokens <= 0)) {
    errors.push('maxTokens must be a positive number');
  }

  return errors;
}

export function isValidContextQueryParams(params: ContextQueryParams): boolean {
  return validateContextQueryParams(params).length === 0;
}
