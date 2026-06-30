import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js'
import type { LunarBackend } from '../backend/types.js'
import { LunarError } from '../backend/types.js'
import { PushSchema } from '../schemas.js'

type Args = { workspace: string; message?: string }

export function createHandler(backend: LunarBackend): (args: Args) => Promise<CallToolResult> {
  return async (args) => {
    try {
      const data = await backend.push({ workspace: args.workspace, message: args.message })
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
    'push',
    {
      title: 'Push Workspace',
      description:
        'Persist the current state of a workspace, committing its changes and producing a new revision. Use to keep the results of work done in a forked or ephemeral workspace. After a successful push the work is durable.',
      inputSchema: PushSchema,
    },
    createHandler(backend),
  )
}
