import { createServer as createHttpServer } from 'node:http'
import type { IncomingMessage, ServerResponse } from 'node:http'
import { StreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/streamableHttp.js'
import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js'
import { createServer } from './server.js'
import { isAuthorized } from './config.js'
import type { RemoteConfig } from './config.js'

// Exported for unit testing without a live socket.
export function createRequestHandler(
  config: RemoteConfig,
  makeServer: () => McpServer,
): (req: IncomingMessage, res: ServerResponse) => Promise<void> {
  return async (req: IncomingMessage, res: ServerResponse): Promise<void> => {
    if (!isAuthorized(req.headers['authorization'], config.token)) {
      res.writeHead(401, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ error: 'unauthorized' }))
      return
    }
    // nyx: stateless mode (sessionIdGenerator: undefined) - no session tracking needed for this MVP
    const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined })
    const server = makeServer()
    await server.connect(transport)
    await transport.handleRequest(req, res)
  }
}

export async function startRemoteServer(
  config: RemoteConfig,
  makeServer: () => McpServer = createServer,
): Promise<{ close: () => Promise<void> }> {
  const handler = createRequestHandler(config, makeServer)
  const httpServer = createHttpServer((req: IncomingMessage, res: ServerResponse) => {
    void handler(req, res)
  })

  await new Promise<void>((resolve, reject) => {
    httpServer.once('error', reject)
    httpServer.listen(config.port, config.host, resolve)
  })

  return {
    close: () =>
      new Promise<void>((resolve, reject) => {
        httpServer.close((err) => (err !== undefined ? reject(err) : resolve()))
      }),
  }
}
