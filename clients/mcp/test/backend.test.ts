import { describe, it, expect, vi, beforeEach } from 'vitest'
import type { CliRunner, CliResult, HttpClient, HttpResponse } from '../src/backend/types.js'
import { LunarError } from '../src/backend/types.js'
import { LunarBackendImpl } from '../src/backend/lunar-backend.js'

function makeCli(overrides?: Partial<CliResult>): CliRunner {
  return {
    run: vi.fn().mockResolvedValue({
      stdout: '',
      stderr: '',
      exitCode: 0,
      ...overrides,
    }),
  }
}

function makeHttp(overrides?: Partial<HttpResponse>): HttpClient {
  return {
    request: vi.fn().mockResolvedValue({
      status: 200,
      body: null,
      ...overrides,
    }),
  }
}

describe('LunarBackendImpl.forkWorkspace', () => {
  it('calls http POST /v1/workspaces/:source/fork and returns Workspace', async () => {
    const http = makeHttp({ status: 201, body: { workspace: 'fork-copy', root: 'abc123' } })
    const backend = new LunarBackendImpl({ cli: makeCli(), http })
    const result = await backend.forkWorkspace({ source: 'ws-1', name: 'fork-copy' })
    expect(http.request).toHaveBeenCalledOnce()
    expect(http.request).toHaveBeenCalledWith(
      'POST',
      '/v1/workspaces/ws-1/fork',
      { new_workspace: 'fork-copy' },
    )
    expect(result.id).toBe('fork-copy')
    expect(result.name).toBe('fork-copy')
    expect(result.status).toBe('ready')
    expect(result.ephemeral).toBe(false)
  })

  it('generates a default fork name when name is not provided', async () => {
    const http = makeHttp({ status: 201, body: { workspace: 'fork-ws-1', root: 'abc123' } })
    const backend = new LunarBackendImpl({ cli: makeCli(), http })
    const result = await backend.forkWorkspace({ source: 'ws-1' })
    expect(http.request).toHaveBeenCalledWith(
      'POST',
      '/v1/workspaces/ws-1/fork',
      { new_workspace: 'fork-ws-1' },
    )
    expect(result.id).toBe('fork-ws-1')
  })

  it('throws LunarError with fork_failed on non-2xx status', async () => {
    const http = makeHttp({ status: 404, body: { error: 'not found' } })
    const backend = new LunarBackendImpl({ cli: makeCli(), http })
    await expect(backend.forkWorkspace({ source: 'missing' })).rejects.toMatchObject({
      code: 'fork_failed',
    })
  })

  it('falls back to the supplied name when response body has no workspace field', async () => {
    const http = makeHttp({ status: 201, body: {} })
    const backend = new LunarBackendImpl({ cli: makeCli(), http })
    const result = await backend.forkWorkspace({ source: 'ws-1', name: 'my-fork' })
    expect(result.id).toBe('my-fork')
  })

  it('throws LunarError with fork_failed when source is empty', async () => {
    const backend = new LunarBackendImpl({ cli: makeCli(), http: makeHttp() })
    await expect(backend.forkWorkspace({ source: '' })).rejects.toMatchObject({
      code: 'fork_failed',
    })
  })
})

describe('LunarBackendImpl.mount', () => {
  it('calls cli with correct argv', async () => {
    const cli = makeCli()
    const backend = new LunarBackendImpl({ cli, http: makeHttp() })
    const result = await backend.mount({ workspace: 'ws-1', path: '/tmp/ws' })
    expect(cli.run).toHaveBeenCalledWith(['mount', 'ws-1', '/tmp/ws'])
    expect(result.mounted).toBe(true)
    expect(result.workspace).toBe('ws-1')
    expect(result.path).toBe('/tmp/ws')
  })

  it('throws LunarError with mount_failed on nonzero exit', async () => {
    const cli = makeCli({ exitCode: 2, stderr: 'busy' })
    const backend = new LunarBackendImpl({ cli, http: makeHttp() })
    await expect(backend.mount({ workspace: 'ws-1', path: '/tmp/ws' })).rejects.toMatchObject({
      code: 'mount_failed',
    })
  })
})

describe('LunarBackendImpl.listWorkspaces', () => {
  let backend: LunarBackendImpl
  let http: HttpClient

  beforeEach(() => {
    http = makeHttp({
      body: {
        workspaces: [{ id: 'ws-1', label: 'main', state: 'ready', ephemeral: false }],
      },
    })
    backend = new LunarBackendImpl({ cli: makeCli(), http })
  })

  it('calls http GET /v1/workspaces without filter', async () => {
    const result = await backend.listWorkspaces({})
    expect(http.request).toHaveBeenCalledWith('GET', '/v1/workspaces')
    expect(result).toHaveLength(1)
    expect(result[0]?.id).toBe('ws-1')
    expect(result[0]?.name).toBe('main')
    expect(result[0]?.status).toBe('ready')
    expect(result[0]?.ephemeral).toBe(false)
  })

  it('calls http GET /v1/workspaces?filter=... with filter', async () => {
    vi.mocked(http.request).mockResolvedValue({ status: 200, body: { workspaces: [] } })
    await backend.listWorkspaces({ filter: 'prod env' })
    expect(http.request).toHaveBeenCalledWith('GET', '/v1/workspaces?filter=prod%20env')
  })

  it('returns empty array when workspaces list is empty', async () => {
    vi.mocked(http.request).mockResolvedValue({ status: 200, body: { workspaces: [] } })
    const result = await backend.listWorkspaces({})
    expect(result).toHaveLength(0)
  })

  it('maps id to name when label is missing', async () => {
    vi.mocked(http.request).mockResolvedValue({
      status: 200,
      body: { workspaces: [{ id: 'ws-2', state: 'ephemeral', ephemeral: true }] },
    })
    const result = await backend.listWorkspaces({})
    expect(result[0]?.name).toBe('ws-2')
    expect(result[0]?.ephemeral).toBe(true)
  })

  it('throws LunarError with list_failed on non-2xx status', async () => {
    vi.mocked(http.request).mockResolvedValue({ status: 503, body: null })
    await expect(backend.listWorkspaces({})).rejects.toMatchObject({ code: 'list_failed' })
  })

  it('throws LunarError with list_failed on unexpected body shape', async () => {
    vi.mocked(http.request).mockResolvedValue({ status: 200, body: [{ id: 'ws-1' }] })
    await expect(backend.listWorkspaces({})).rejects.toMatchObject({ code: 'list_failed' })
  })
})

describe('LunarBackendImpl.push', () => {
  it('reads the ref then puts it back, returning pushed=true with the revision', async () => {
    const http = makeHttp()
    vi.mocked(http.request)
      .mockResolvedValueOnce({ status: 200, body: { root: 'abc123' } })
      .mockResolvedValueOnce({ status: 200, body: null })
    const backend = new LunarBackendImpl({ cli: makeCli(), http })
    const result = await backend.push({ workspace: 'ws-1', message: 'deploy' })
    expect(http.request).toHaveBeenNthCalledWith(1, 'GET', '/v1/ref/ws-1')
    expect(http.request).toHaveBeenNthCalledWith(2, 'PUT', '/v1/ref/ws-1', { root: 'abc123' })
    expect(result.pushed).toBe(true)
    expect(result.revision).toBe('abc123')
    expect(result.workspace).toBe('ws-1')
  })

  it('throws LunarError with push_failed when ref GET fails', async () => {
    const http = makeHttp()
    vi.mocked(http.request).mockResolvedValueOnce({ status: 404, body: null })
    const backend = new LunarBackendImpl({ cli: makeCli(), http })
    await expect(backend.push({ workspace: 'ws-1' })).rejects.toMatchObject({ code: 'push_failed' })
  })

  it('throws LunarError with push_failed when ref body has no root', async () => {
    const http = makeHttp()
    vi.mocked(http.request).mockResolvedValueOnce({ status: 200, body: {} })
    const backend = new LunarBackendImpl({ cli: makeCli(), http })
    await expect(backend.push({ workspace: 'ws-1' })).rejects.toMatchObject({ code: 'push_failed' })
  })

  it('throws LunarError with push_failed when PUT fails', async () => {
    const http = makeHttp()
    vi.mocked(http.request)
      .mockResolvedValueOnce({ status: 200, body: { root: 'abc123' } })
      .mockResolvedValueOnce({ status: 409, body: null })
    const backend = new LunarBackendImpl({ cli: makeCli(), http })
    await expect(backend.push({ workspace: 'ws-1' })).rejects.toMatchObject({ code: 'push_failed' })
  })

  it('throws LunarError with push_failed when workspace is empty', async () => {
    const backend = new LunarBackendImpl({ cli: makeCli(), http: makeHttp() })
    await expect(backend.push({ workspace: '' })).rejects.toMatchObject({ code: 'push_failed' })
  })
})

describe('LunarBackendImpl.grantAccess', () => {
  it('calls http POST /workspaces/{ws}/access with body', async () => {
    const http = makeHttp({ status: 201, body: {} })
    const backend = new LunarBackendImpl({ cli: makeCli(), http })
    const result = await backend.grantAccess({ workspace: 'ws-1', grantee: 'bob@x.com', role: 'write' })
    expect(http.request).toHaveBeenCalledWith('POST', '/workspaces/ws-1/access', {
      grantee: 'bob@x.com',
      role: 'write',
    })
    expect(result.grantee).toBe('bob@x.com')
    expect(result.role).toBe('write')
  })

  it('throws LunarError with grant_failed on non-2xx status', async () => {
    const http = makeHttp({ status: 404, body: null })
    const backend = new LunarBackendImpl({ cli: makeCli(), http })
    await expect(
      backend.grantAccess({ workspace: 'ws-1', grantee: 'x', role: 'admin' }),
    ).rejects.toMatchObject({ code: 'grant_failed' })
  })
})

describe('LunarBackendImpl.destroy', () => {
  it('calls cli with destroy argv including --yes', async () => {
    const cli = makeCli()
    const backend = new LunarBackendImpl({ cli, http: makeHttp() })
    const result = await backend.destroy({ workspace: 'ws-1' })
    expect(cli.run).toHaveBeenCalledWith(['destroy', 'ws-1', '--yes'])
    expect(result.destroyed).toBe(true)
    expect(result.workspace).toBe('ws-1')
  })

  it('throws LunarError with destroy_failed on nonzero exit', async () => {
    const cli = makeCli({ exitCode: 1, stderr: 'workspace locked' })
    const backend = new LunarBackendImpl({ cli, http: makeHttp() })
    await expect(backend.destroy({ workspace: 'ws-1' })).rejects.toMatchObject({
      code: 'destroy_failed',
    })
  })
})
