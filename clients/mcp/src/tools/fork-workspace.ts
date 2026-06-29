import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js'
import type { LunarBackend } from '../backend/types.js'
import { LunarError } from '../backend/types.js'
import { ForkWorkspaceSchema } from '../schemas.js'

type Args = { source: string; name?: string }

export function createHandler(backend: LunarBackend): (args: Args) => Promise<CallToolResult> {
  return async (args) => {
    try {
      const data = await backend.forkWorkspace({ source: args.source, name: args.name })
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
    'fork_workspace',
    {
      title: 'Fork Workspace',
      description:
        'Instantly clone an existing workspace into an isolated copy so you can run risky, experimental, or parallel work without touching the original. Use this before any change you might want to throw away. The fork starts as an ephemeral workspace you can later push to keep, or destroy to discard.',
      inputSchema: ForkWorkspaceSchema,
    },
    createHandler(backend),
  )
}
