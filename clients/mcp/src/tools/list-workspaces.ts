import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js'
import type { LunarBackend } from '../backend/types.js'
import { LunarError } from '../backend/types.js'
import { ListWorkspacesSchema } from '../schemas.js'

type Args = { filter?: string }

export function createHandler(backend: LunarBackend): (args: Args) => Promise<CallToolResult> {
  return async (args) => {
    try {
      const data = await backend.listWorkspaces({ filter: args.filter })
      return {
        content: [{ type: 'text', text: JSON.stringify({ ok: true, data }) }],
      }
    } catch (e) {
      const code = e instanceof LunarError ? e.code : 'unknown_error'
      const message = e instanceof Error ? e.message : String(e)
      return {
        content: [{ type: 'text', text: JSON.stringify({ ok: false, error: { code, message } }) }],
        isError: true,
      }
    }
  }
}

export function register(server: McpServer, backend: LunarBackend): void {
  server.registerTool(
    'list_workspaces',
    {
      title: 'List Workspaces',
      description:
        'List all workspaces with their id, name, status, and whether they are ephemeral. Use to discover what workspaces exist before forking, mounting, pushing, granting access, or destroying one. Optional filter narrows by name or status.',
      inputSchema: ListWorkspacesSchema,
    },
    createHandler(backend),
  )
}
