import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js'
import type { LunarBackend } from '../backend/types.js'
import { LunarError } from '../backend/types.js'
import { DestroySchema } from '../schemas.js'

type Args = { workspace: string }

export function createHandler(backend: LunarBackend): (args: Args) => Promise<CallToolResult> {
  return async (args) => {
    try {
      const data = await backend.destroy({ workspace: args.workspace })
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
    'destroy',
    {
      title: 'Destroy Workspace',
      description:
        'Permanently drop an ephemeral workspace and all of its state. Use to discard a fork you do not want to keep or to clean up after parallel work. This is irreversible; anything not pushed is lost.',
      inputSchema: DestroySchema,
    },
    createHandler(backend),
  )
}
