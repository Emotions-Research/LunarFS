export interface CliResult { stdout: string; stderr: string; exitCode: number }
export interface CliRunner { run(args: string[]): Promise<CliResult> }
export interface HttpResponse { status: number; body: unknown }
export interface HttpClient { request(method: string, path: string, body?: unknown): Promise<HttpResponse> }
export interface Workspace { id: string; name: string; status: string; ephemeral: boolean }
export interface ForkInput { source: string; name?: string }
export interface MountInput { workspace: string; path: string }
export interface ListInput { filter?: string }
export interface PushInput { workspace: string; message?: string }
export type AccessRole = 'read' | 'write' | 'admin'
export interface GrantInput { workspace: string; grantee: string; role: AccessRole }
export interface DestroyInput { workspace: string }

export interface LunarBackend {
  forkWorkspace(input: ForkInput): Promise<Workspace>
  mount(input: MountInput): Promise<{ workspace: string; path: string; mounted: boolean }>
  listWorkspaces(input: ListInput): Promise<Workspace[]>
  push(input: PushInput): Promise<{ workspace: string; pushed: boolean; revision?: string }>
  grantAccess(input: GrantInput): Promise<{ workspace: string; grantee: string; role: AccessRole }>
  destroy(input: DestroyInput): Promise<{ workspace: string; destroyed: boolean }>
}

export class LunarError extends Error {
  constructor(public code: string, message: string) {
    super(message)
    this.name = 'LunarError'
  }
}
