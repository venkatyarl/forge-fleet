import { ContextQueryParams } from '../../schemas/context_query';

export const GET_CONTEXT_TOOL_NAME = 'forge_fleet.get_context';

export const GET_CONTEXT_TOOL_DEFINITION = {
  description: 'Fetch context chunks from ForgeFleet local context storage.',
  inputSchema: {
    type: 'object',
    properties: {
      id: { type: 'string', description: 'Context source UUID.' },
      name: { type: 'string', description: 'Context source title or URI.' },
      query: { type: 'string', description: 'Text to find in the context source.' },
      maxTokens: { type: 'number', description: 'Maximum approximate result size in tokens.' },
      verbose: { type: 'boolean', description: 'Include verbose context retrieval output.' },
    },
    required: ['query'],
    anyOf: [{ required: ['id'] }, { required: ['name'] }],
    additionalProperties: false,
  },
} as const;

export interface ContextFetcher {
  fetchContext(query: ContextQueryParams): Promise<string[]>;
}

export interface McpTextContent {
  type: 'text';
  text: string;
}

export interface McpToolResult {
  content: McpTextContent[];
}

function contextQueryFromArguments(args: unknown): ContextQueryParams {
  if (typeof args !== 'object' || args === null || Array.isArray(args)) {
    throw new Error('invalid context query: arguments must be an object');
  }

  const input = args as Record<string, unknown>;
  return {
    id: typeof input.id === 'string' ? input.id : undefined,
    name: typeof input.name === 'string' ? input.name : undefined,
    query: typeof input.query === 'string' ? input.query : '',
    maxTokens: typeof input.maxTokens === 'number' ? input.maxTokens : undefined,
    verbose: typeof input.verbose === 'boolean' ? input.verbose : undefined,
  };
}

export async function handleGetContext(
  args: unknown,
  dbFetcher: ContextFetcher,
): Promise<McpToolResult> {
  const query = contextQueryFromArguments(args);
  const chunks = await dbFetcher.fetchContext(query);

  return {
    content: [
      {
        type: 'text',
        text: JSON.stringify({ query: query.query, chunks }),
      },
    ],
  };
}
