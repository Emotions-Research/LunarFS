import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import type { LunarBackend } from './backend/types.js'
import { LunarBackendImpl } from './backend/lunar-backend.js'
import { register as registerFork } from './tools/fork-workspace.js'
import { register as registerMount } from './tools/mount.js'
import { register as registerList } from './tools/list-workspaces.js'
import { register as registerPush } from './tools/push.js'
import { register as registerGrant } from './tools/grant-access.js'
import { register as registerDestroy } from './tools/destroy.js'

export function createServer(backend: LunarBackend = new LunarBackendImpl()): McpServer {
  const server = new McpServer({ name: 'lunarfs-mcp', version: '0.1.0' })
  registerFork(server, backend)
  registerMount(server, backend)
  registerList(server, backend)
  registerPush(server, backend)
  registerGrant(server, backend)
  registerDestroy(server, backend)
  return server
}
