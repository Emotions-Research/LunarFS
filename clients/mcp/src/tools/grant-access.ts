import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js'
import type { LunarBackend } from '../backend/types.js'
import { LunarError } from '../backend/types.js'
import { GrantAccessSchema } from '../schemas.js'
import type { AccessRole } from '../backend/types.js'

type Args = { workspace: string; grantee: string; role: AccessRole }

export function createHandler(backend: LunarBackend): (args: Args) => Promise<CallToolResult> {
  return async (args) => {
    try {
      const data = await backend.grantAccess({
        workspace: args.workspace,
        grantee: args.grantee,
        role: args.role,
      })
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
    'grant_access',
    {
      title: 'Grant Access',
      description:
        'Grant another user access to a workspace at a chosen role (read, write, or admin). Use to share a workspace or hand off ownership. Roles are cumulative in capability: read can view, write can modify, admin can manage access.',
      inputSchema: GrantAccessSchema,
    },
    createHandler(backend),
  )
}
