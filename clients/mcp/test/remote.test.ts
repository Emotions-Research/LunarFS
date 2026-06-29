import { describe, it, expect, vi, beforeEach } from 'vitest'
import type { IncomingMessage, ServerResponse, IncomingHttpHeaders } from 'node:http'
import { resolveConfig, isAuthorized } from '../src/config.js'
import { createRequestHandler } from '../src/remote.js'
import { createServer } from '../src/server.js'
import type { LunarBackend, Workspace } from '../src/backend/types.js'
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js'
import { Client } from '@modelcontextprotocol/sdk/client/index.js'

// --- helpers ---

function makeBackend(): LunarBackend {
  return {
    forkWorkspace: vi.fn(),
    mount: vi.fn(),
    listWorkspaces: vi.fn(),
    push: vi.fn(),
    grantAccess: vi.fn(),
    destroy: vi.fn(),
  }
}

function mockReq(headers: IncomingHttpHeaders): IncomingMessage {
  return { headers } as unknown as IncomingMessage
}

function mockRes(): { res: ServerResponse; status: () => number; body: () => string } {
  let capturedStatus = 0
  let capturedBody = ''
  const res = {
    writeHead(code: number) { capturedStatus = code },
    end(data: string) { capturedBody = data },
  } as unknown as ServerResponse
  return { res, status: () => capturedStatus, body: () => capturedBody }
}

// --- (a) transport selection ---

describe('resolveConfig: transport selection', () => {
  it('defaults to stdio when LUNAR_MCP_REMOTE is unset', () => {
    expect(resolveConfig({}).mode).toBe('stdio')
  })

  it('resolves remote with defaults when flag and token present', () => {
    const cfg = resolveConfig({ LUNAR_MCP_REMOTE: 'true', LUNAR_MCP_TOKEN: 'secret' })
    expect(cfg.mode).toBe('remote')
    expect(cfg.remote?.host).toBe('127.0.0.1')
    expect(cfg.remote?.port).toBe(3000)
    expect(cfg.remote?.token).toBe('secret')
  })

  it('resolves custom host and port', () => {
    const cfg = resolveConfig({
      LUNAR_MCP_REMOTE: '1',
      LUNAR_MCP_TOKEN: 't',
      LUNAR_MCP_HOST: '0.0.0.0',
      LUNAR_MCP_PORT: '8080',
    })
    expect(cfg.remote?.host).toBe('0.0.0.0')
    expect(cfg.remote?.port).toBe(8080)
  })

  it('throws when flag is set but token is missing', () => {
    expect(() => resolveConfig({ LUNAR_MCP_REMOTE: 'true' })).toThrow(
      'LUNAR_MCP_REMOTE is set but LUNAR_MCP_TOKEN is empty',
    )
  })

  it('returns stdio when LUNAR_MCP_REMOTE is falsey', () => {
    const cfg = resolveConfig({ LUNAR_MCP_REMOTE: '0', LUNAR_MCP_TOKEN: 'x' })
    expect(cfg.mode).toBe('stdio')
  })

  it('accepts yes as truthy', () => {
    const cfg = resolveConfig({ LUNAR_MCP_REMOTE: 'yes', LUNAR_MCP_TOKEN: 'tok' })
    expect(cfg.mode).toBe('remote')
  })

  it('falls back to port 3000 on invalid LUNAR_MCP_PORT', () => {
    const cfg = resolveConfig({ LUNAR_MCP_REMOTE: '1', LUNAR_MCP_TOKEN: 't', LUNAR_MCP_PORT: 'abc' })
    expect(cfg.remote?.port).toBe(3000)
  })
})

// --- (b) auth rejection ---

describe('isAuthorized', () => {
  it('rejects missing header', () => {
    expect(isAuthorized(undefined, 'secret')).toBe(false)
  })

  it('rejects wrong token', () => {
    expect(isAuthorized('Bearer wrong', 'secret')).toBe(false)
  })

  it('rejects missing Bearer prefix', () => {
    expect(isAuthorized('secret', 'secret')).toBe(false)
  })

  it('accepts correct Bearer token', () => {
    expect(isAuthorized('Bearer secret', 'secret')).toBe(true)
  })

  it('rejects token with length mismatch (timing-safe branch)', () => {
    expect(isAuthorized('Bearer short', 'longer-token')).toBe(false)
  })
})

// --- (c) token-authenticated request reaches a tool via InMemoryTransport ---

describe('createServer via InMemoryTransport: tool reachability', () => {
  it('routes fork_workspace through createServer and returns ok envelope', async () => {
    const backend = makeBackend()
    const forkResult: Workspace = { id: 'ws-2', name: 'fork', status: 'ready', ephemeral: true }
    vi.mocked(backend.forkWorkspace).mockResolvedValue(forkResult)

    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair()
    const server = createServer(backend)
    await server.connect(serverTransport)

    const client = new Client({ name: 'test-client', version: '1.0.0' })
    await client.connect(clientTransport)

    const result = await client.callTool({ name: 'fork_workspace', arguments: { source: 'ws-1' } })

    expect(backend.forkWorkspace).toHaveBeenCalledOnce()

    const textBlock = result.content.find((c) => c.type === 'text')
    expect(textBlock).toBeDefined()
    const parsed = JSON.parse((textBlock as { type: 'text'; text: string }).text) as {
      ok: boolean
      data: { id: string }
    }
    expect(parsed.ok).toBe(true)
    expect(parsed.data.id).toBe('ws-2')

    await client.close()
  })
})

// --- (d) auth gate blocks dispatch ---

describe('createRequestHandler: auth gate', () => {
  const config = { host: '127.0.0.1', port: 3000, token: 'secret' }

  beforeEach(() => {
    vi.clearAllMocks()
  })

  it('rejects missing Authorization with 401 and never calls makeServer', async () => {
    const makeServer = vi.fn()
    const handler = createRequestHandler(config, makeServer)
    const { res, status, body } = mockRes()

    await handler(mockReq({}), res)

    expect(status()).toBe(401)
    expect(JSON.parse(body())).toEqual({ error: 'unauthorized' })
    expect(makeServer).not.toHaveBeenCalled()
  })

  it('rejects invalid token with 401 and never calls makeServer', async () => {
    const makeServer = vi.fn()
    const handler = createRequestHandler(config, makeServer)
    const { res, status, body } = mockRes()

    await handler(mockReq({ authorization: 'Bearer wrong-token' }), res)

    expect(status()).toBe(401)
    expect(JSON.parse(body())).toEqual({ error: 'unauthorized' })
    expect(makeServer).not.toHaveBeenCalled()
  })

  it('rejects no-prefix header with 401 and never calls makeServer', async () => {
    const makeServer = vi.fn()
    const handler = createRequestHandler(config, makeServer)
    const { res, status } = mockRes()

    await handler(mockReq({ authorization: 'secret' }), res)

    expect(status()).toBe(401)
    expect(makeServer).not.toHaveBeenCalled()
  })
})
