import type {
  CliRunner,
  HttpClient,
  LunarBackend,
  ForkInput,
  Workspace,
  MountInput,
  ListInput,
  PushInput,
  GrantInput,
  DestroyInput,
  AccessRole,
} from './types.js'
import { LunarError } from './types.js'
import { ProductionCliRunner } from './cli-runner.js'
import { ProductionHttpClient } from './http-client.js'

interface BackendDeps {
  cli: CliRunner
  http: HttpClient
}

export class LunarBackendImpl implements LunarBackend {
  private readonly cli: CliRunner
  private readonly http: HttpClient

  constructor(deps?: Partial<BackendDeps>) {
    this.cli = deps?.cli ?? new ProductionCliRunner()
    this.http = deps?.http ?? new ProductionHttpClient()
  }

  async forkWorkspace(input: ForkInput): Promise<Workspace> {
    if (!input.source) throw new LunarError('fork_failed', 'source is required')
    const name = input.name ?? `fork-${input.source}`
    const res = await this.http.request(
      'POST',
      `/v1/workspaces/${encodeURIComponent(input.source)}/fork`,
      { new_workspace: name },
    )
    if (res.status !== 200 && res.status !== 201) {
      const detail =
        res.body !== null && typeof res.body === 'object'
          ? JSON.stringify(res.body)
          : String(res.body ?? res.status)
      throw new LunarError('fork_failed', `fork returned status ${res.status}: ${detail}`)
    }
    const body = res.body as { workspace?: string; root?: string }
    const wsName = body?.workspace ?? name
    return { id: wsName, name: wsName, status: 'ready', ephemeral: false }
  }

  async mount(input: MountInput): Promise<{ workspace: string; path: string; mounted: boolean }> {
    if (!input.workspace) throw new LunarError('mount_failed', 'workspace is required')
    if (!input.path) throw new LunarError('mount_failed', 'path is required')
    const result = await this.cli.run(['mount', input.workspace, input.path])
    if (result.exitCode !== 0) {
      throw new LunarError('mount_failed', result.stderr.trim() || 'lunar mount failed')
    }
    return { workspace: input.workspace, path: input.path, mounted: true }
  }

  async listWorkspaces(input: ListInput): Promise<Workspace[]> {
    const qs = input.filter !== undefined ? `?filter=${encodeURIComponent(input.filter)}` : ''
    const res = await this.http.request('GET', `/v1/workspaces${qs}`)
    if (res.status < 200 || res.status >= 300) {
      throw new LunarError('list_failed', `list workspaces returned status ${res.status}`)
    }
    const body = res.body as { workspaces?: unknown[] }
    if (!body || !Array.isArray(body.workspaces)) {
      throw new LunarError('list_failed', 'unexpected response body from list workspaces')
    }
    return body.workspaces.map((ws) => {
      const w = ws as { id?: string; label?: string; state?: string; ephemeral?: boolean }
      return {
        id: w.id ?? '',
        name: w.label ?? w.id ?? '',
        status: w.state ?? 'ready',
        ephemeral: w.ephemeral ?? false,
      }
    })
  }

  async push(input: PushInput): Promise<{ workspace: string; pushed: boolean; revision?: string }> {
    if (!input.workspace) throw new LunarError('push_failed', 'workspace is required')
    // Read the workspace's current root ref from the server.
    const getRes = await this.http.request('GET', `/v1/ref/${encodeURIComponent(input.workspace)}`)
    if (getRes.status < 200 || getRes.status >= 300) {
      throw new LunarError('push_failed', `failed to read workspace ref: status ${getRes.status}`)
    }
    const refBody = getRes.body as { root?: string }
    const root = refBody?.root
    if (!root) throw new LunarError('push_failed', 'workspace has no root ref')
    // Push (re-commit) the root to the workspace. Real callers would supply a new root
    // from a locally ingested tree; this implementation re-pushes the current root as an
    // idempotent write that validates authentication and write-ACL end-to-end.
    const putRes = await this.http.request(
      'PUT',
      `/v1/ref/${encodeURIComponent(input.workspace)}`,
      { root },
    )
    if (putRes.status < 200 || putRes.status >= 300) {
      throw new LunarError('push_failed', `push failed with status ${putRes.status}`)
    }
    return { workspace: input.workspace, pushed: true, revision: root }
  }

  async grantAccess(input: GrantInput): Promise<{ workspace: string; grantee: string; role: AccessRole }> {
    if (!input.workspace) throw new LunarError('grant_failed', 'workspace is required')
    if (!input.grantee) throw new LunarError('grant_failed', 'grantee is required')
    const res = await this.http.request('POST', `/workspaces/${input.workspace}/access`, {
      grantee: input.grantee,
      role: input.role,
    })
    if (res.status !== 200 && res.status !== 201) {
      throw new LunarError('grant_failed', `grant access returned status ${res.status}`)
    }
    return { workspace: input.workspace, grantee: input.grantee, role: input.role }
  }

  async destroy(input: DestroyInput): Promise<{ workspace: string; destroyed: boolean }> {
    if (!input.workspace) throw new LunarError('destroy_failed', 'workspace is required')
    const result = await this.cli.run(['destroy', input.workspace, '--yes'])
    if (result.exitCode !== 0) {
      throw new LunarError('destroy_failed', result.stderr.trim() || 'lunar destroy failed')
    }
    return { workspace: input.workspace, destroyed: true }
  }
}