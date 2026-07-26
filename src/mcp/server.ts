import {
  ContextFetcher,
  GET_CONTEXT_TOOL_DEFINITION,
  GET_CONTEXT_TOOL_NAME,
  McpToolResult,
  handleGetContext,
} from './handlers/context';

export interface McpServer {
  registerTool(
    name: string,
    definition: typeof GET_CONTEXT_TOOL_DEFINITION,
    handler: (args: unknown) => Promise<McpToolResult>,
  ): void;
}

export function registerContextTool(server: McpServer, dbFetcher: ContextFetcher): void {
  server.registerTool(GET_CONTEXT_TOOL_NAME, GET_CONTEXT_TOOL_DEFINITION, (args) =>
    handleGetContext(args, dbFetcher),
  );
}

export function registerMcpTools(server: McpServer, dbFetcher: ContextFetcher): void {
  registerContextTool(server, dbFetcher);
}
