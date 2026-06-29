#!/usr/bin/env node
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { createServer } from './server.js'
import { resolveConfig } from './config.js'
import { startRemoteServer } from './remote.js'

;(async () => {
  const cfg = resolveConfig()
  if (cfg.mode === 'remote' && cfg.remote != null) {
    await startRemoteServer(cfg.remote)
    process.stderr.write(`lunarfs-mcp: listening on http://${cfg.remote.host}:${cfg.remote.port}\n`)
  } else {
    const server = createServer()
    const transport = new StdioServerTransport()
    server.connect(transport).catch((err: unknown) => {
      process.stderr.write(`lunarfs-mcp: fatal: ${String(err)}\n`)
      process.exit(1)
    })
  }
})().catch((err: unknown) => {
  process.stderr.write(`lunarfs-mcp: fatal: ${String(err)}\n`)
  process.exit(1)
})
