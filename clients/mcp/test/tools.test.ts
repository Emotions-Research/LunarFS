import { describe, it, expect, vi, beforeEach } from 'vitest'
import { z } from 'zod'
import { LunarError } from '../src/backend/types.js'
import type { LunarBackend, Workspace, AccessRole } from '../src/backend/types.js'
import { createHandler as forkHandler } from '../src/tools/fork-workspace.js'
import { createHandler as mountHandler } from '../src/tools/mount.js'
import { createHandler as listHandler } from '../src/tools/list-workspaces.js'
import { createHandler as pushHandler } from '../src/tools/push.js'
import { createHandler as grantHandler } from '../src/tools/grant-access.js'
import { createHandler as destroyHandler } from '../src/tools/destroy.js'
import {
  ForkWorkspaceSchema,
  MountSchema,
  ListWorkspacesSchema,
  PushSchema,
  GrantAccessSchema,
  DestroySchema,
} from '../src/schemas.js'

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

const WORKSPACE: Workspace = { id: 'ws-1', name: 'main', status: 'ready', ephemeral: false }

function parseResult(text: string): unknown {
  return JSON.parse(text)
}

describe('fork_workspace tool', () => {
  let backend: LunarBackend

  beforeEach(() => {
    backend = makeBackend()
  })

  it('calls forkWorkspace with correct args and returns ok envelope', async () => {
    vi.mocked(backend.forkWorkspace).mockResolvedValue({ ...WORKSPACE, id: 'ws-2', ephemeral: true })
    const result = await forkHandler(backend)({ source: 'ws-1', name: 'fork-copy' })
    expect(backend.forkWorkspace).toHaveBeenCalledOnce()
    expect(backend.forkWorkspace).toHaveBeenCalledWith({ source: 'ws-1', name: 'fork-copy' })
    expect(result.isError).toBeFalsy()
    const body = parseResult(result.content[0].text) as { ok: boolean; data: Workspace }
    expect(body.ok).toBe(true)
    expect(body.data.id).toBe('ws-2')
  })

  it('omits name when not provided', async () => {
    vi.mocked(backend.forkWorkspace).mockResolvedValue({ ...WORKSPACE, id: 'ws-3', ephemeral: true })
    await forkHandler(backend)({ source: 'ws-1' })
    expect(backend.forkWorkspace).toHaveBeenCalledWith({ source: 'ws-1', name: undefined })
  })

  it('returns isError envelope on LunarError', async () => {
    vi.mocked(backend.forkWorkspace).mockRejectedValue(new LunarError('fork_failed', 'bad fork'))
    const result = await forkHandler(backend)({ source: 'ws-1' })
    expect(result.isError).toBe(true)
    const body = parseResult(result.content[0].text) as { ok: boolean; error: { code: string; message: string } }
    expect(body.ok).toBe(false)
    expect(body.error.code).toBe('fork_failed')
    expect(body.error.message).toBe('bad fork')
  })

  it('returns isError envelope on generic Error', async () => {
    vi.mocked(backend.forkWorkspace).mockRejectedValue(new Error('network down'))
    const result = await forkHandler(backend)({ source: 'ws-1' })
    expect(result.isError).toBe(true)
    const body = parseResult(result.content[0].text) as { ok: boolean; error: { code: string } }
    expect(body.error.code).toBe('unknown_error')
  })

  it('schema rejects missing required source field', () => {
    const schema = z.object(ForkWorkspaceSchema)
    const r = schema.safeParse({ name: 'x' })
    expect(r.success).toBe(false)
  })

  it('schema rejects empty source string', () => {
    const schema = z.object(ForkWorkspaceSchema)
    const r = schema.safeParse({ source: '' })
    expect(r.success).toBe(false)
  })
})

describe('mount tool', () => {
  let backend: LunarBackend

  beforeEach(() => {
    backend = makeBackend()
  })

  it('calls mount with correct args and returns ok envelope', async () => {
    vi.mocked(backend.mount).mockResolvedValue({ workspace: 'ws-1', path: '/tmp/ws', mounted: true })
    const result = await mountHandler(backend)({ workspace: 'ws-1', path: '/tmp/ws' })
    expect(backend.mount).toHaveBeenCalledWith({ workspace: 'ws-1', path: '/tmp/ws' })
    expect(result.isError).toBeFalsy()
    const body = parseResult(result.content[0].text) as { ok: boolean; data: { mounted: boolean } }
    expect(body.ok).toBe(true)
    expect(body.data.mounted).toBe(true)
  })

  it('returns isError envelope on LunarError', async () => {
    vi.mocked(backend.mount).mockRejectedValue(new LunarError('mount_failed', 'already mounted'))
    const result = await mountHandler(backend)({ workspace: 'ws-1', path: '/tmp/ws' })
    expect(result.isError).toBe(true)
    const body = parseResult(result.content[0].text) as { ok: boolean; error: { code: string } }
    expect(body.error.code).toBe('mount_failed')
  })

  it('schema rejects missing path', () => {
    const schema = z.object(MountSchema)
    const r = schema.safeParse({ workspace: 'ws-1' })
    expect(r.success).toBe(false)
  })
})

describe('list_workspaces tool', () => {
  let backend: LunarBackend

  beforeEach(() => {
    backend = makeBackend()
  })

  it('calls listWorkspaces and returns ok envelope with array', async () => {
    vi.mocked(backend.listWorkspaces).mockResolvedValue([WORKSPACE])
    const result = await listHandler(backend)({})
    expect(backend.listWorkspaces).toHaveBeenCalledWith({ filter: undefined })
    const body = parseResult(result.content[0].text) as { ok: boolean; data: Workspace[] }
    expect(body.ok).toBe(true)
    expect(body.data).toHaveLength(1)
  })

  it('passes filter when provided', async () => {
    vi.mocked(backend.listWorkspaces).mockResolvedValue([])
    await listHandler(backend)({ filter: 'prod' })
    expect(backend.listWorkspaces).toHaveBeenCalledWith({ filter: 'prod' })
  })

  it('returns empty array when no workspaces', async () => {
    vi.mocked(backend.listWorkspaces).mockResolvedValue([])
    const result = await listHandler(backend)({})
    const body = parseResult(result.content[0].text) as { ok: boolean; data: Workspace[] }
    expect(body.data).toHaveLength(0)
  })

  it('returns isError envelope on LunarError', async () => {
    vi.mocked(backend.listWorkspaces).mockRejectedValue(new LunarError('list_failed', 'unavailable'))
    const result = await listHandler(backend)({})
    expect(result.isError).toBe(true)
  })

  it('schema passes with no filter', () => {
    const schema = z.object(ListWorkspacesSchema)
    expect(schema.safeParse({}).success).toBe(true)
  })
})

describe('push tool', () => {
  let backend: LunarBackend

  beforeEach(() => {
    backend = makeBackend()
  })

  it('calls push with correct args and returns ok envelope', async () => {
    vi.mocked(backend.push).mockResolvedValue({ workspace: 'ws-1', pushed: true, revision: 'rev-abc' })
    const result = await pushHandler(backend)({ workspace: 'ws-1', message: 'deploy' })
    expect(backend.push).toHaveBeenCalledWith({ workspace: 'ws-1', message: 'deploy' })
    expect(result.isError).toBeFalsy()
    const body = parseResult(result.content[0].text) as { ok: boolean; data: { revision: string } }
    expect(body.data.revision).toBe('rev-abc')
  })

  it('omits message when not provided', async () => {
    vi.mocked(backend.push).mockResolvedValue({ workspace: 'ws-1', pushed: true })
    await pushHandler(backend)({ workspace: 'ws-1' })
    expect(backend.push).toHaveBeenCalledWith({ workspace: 'ws-1', message: undefined })
  })

  it('returns isError envelope on LunarError', async () => {
    vi.mocked(backend.push).mockRejectedValue(new LunarError('push_failed', 'conflict'))
    const result = await pushHandler(backend)({ workspace: 'ws-1' })
    expect(result.isError).toBe(true)
  })

  it('schema rejects missing workspace', () => {
    const schema = z.object(PushSchema)
    expect(schema.safeParse({}).success).toBe(false)
  })
})

describe('grant_access tool', () => {
  let backend: LunarBackend

  beforeEach(() => {
    backend = makeBackend()
  })

  it('calls grantAccess with correct args and returns ok envelope', async () => {
    vi.mocked(backend.grantAccess).mockResolvedValue({ workspace: 'ws-1', grantee: 'bob@x.com', role: 'write' })
    const result = await grantHandler(backend)({ workspace: 'ws-1', grantee: 'bob@x.com', role: 'write' })
    expect(backend.grantAccess).toHaveBeenCalledWith({ workspace: 'ws-1', grantee: 'bob@x.com', role: 'write' })
    expect(result.isError).toBeFalsy()
    const body = parseResult(result.content[0].text) as { ok: boolean; data: { role: AccessRole } }
    expect(body.data.role).toBe('write')
  })

  it('returns isError envelope on LunarError', async () => {
    vi.mocked(backend.grantAccess).mockRejectedValue(new LunarError('grant_failed', 'not found'))
    const result = await grantHandler(backend)({ workspace: 'ws-1', grantee: 'x', role: 'read' })
    expect(result.isError).toBe(true)
    const body = parseResult(result.content[0].text) as { ok: boolean; error: { code: string } }
    expect(body.error.code).toBe('grant_failed')
  })

  it('schema rejects missing grantee', () => {
    const schema = z.object(GrantAccessSchema)
    const r = schema.safeParse({ workspace: 'ws-1', role: 'read' })
    expect(r.success).toBe(false)
  })

  it('schema rejects invalid role', () => {
    const schema = z.object(GrantAccessSchema)
    const r = schema.safeParse({ workspace: 'ws-1', grantee: 'bob', role: 'owner' })
    expect(r.success).toBe(false)
  })
})

describe('destroy tool', () => {
  let backend: LunarBackend

  beforeEach(() => {
    backend = makeBackend()
  })

  it('calls destroy with correct args and returns ok envelope', async () => {
    vi.mocked(backend.destroy).mockResolvedValue({ workspace: 'ws-1', destroyed: true })
    const result = await destroyHandler(backend)({ workspace: 'ws-1' })
    expect(backend.destroy).toHaveBeenCalledWith({ workspace: 'ws-1' })
    expect(result.isError).toBeFalsy()
    const body = parseResult(result.content[0].text) as { ok: boolean; data: { destroyed: boolean } }
    expect(body.data.destroyed).toBe(true)
  })

  it('returns isError envelope on LunarError', async () => {
    vi.mocked(backend.destroy).mockRejectedValue(new LunarError('destroy_failed', 'not found'))
    const result = await destroyHandler(backend)({ workspace: 'ws-1' })
    expect(result.isError).toBe(true)
  })

  it('schema rejects missing workspace', () => {
    const schema = z.object(DestroySchema)
    expect(schema.safeParse({}).success).toBe(false)
  })
})
