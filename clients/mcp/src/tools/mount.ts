import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js'
import type { LunarBackend } from '../backend/types.js'
import { LunarError } from '../backend/types.js'
import { MountSchema } from '../schemas.js'

type Args = { workspace: string; path: string }

export function createHandler(backend: LunarBackend): (args: Args) => Promise<CallToolResult> {
  return async (args) => {
    try {
      const data = await backend.mount({ workspace: args.workspace, path: args.path })
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
    'mount',
    {
      title: 'Mount Workspace',
      description:
        'Mount a workspace onto a local path so its files become visible and editable on disk. Use after fork_workspace or when you need to read or edit a workspace locally. Does not copy data; it attaches the live workspace at the given path.',
      inputSchema: MountSchema,
    },
    createHandler(backend),
  )
}
