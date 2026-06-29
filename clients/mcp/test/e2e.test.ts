/**
 * Live STDIO JSON-RPC end-to-end test for lunarfs-mcp.
 *
 * Gates on LUNAR_MCP_E2E=1. When unset the test returns immediately (clean skip).
 * When set it:
 *   1. Resolves the built lunar + lunarfs-mcp binaries.
 *   2. Creates unique temp dirs for the object store and the SQLite identity db.
 *   3. Starts `lunar serve` and waits for real HTTP readiness.
 *   4. Seeds the db with exactly the same rows as test/multidevice/run.sh.
 *   5. Ingests a seed file into the local CAS, then pushes it to the "demo"
 *      workspace so the blob fork endpoint has a ref to copy.
 *   6. Spawns `lunarfs-mcp` over STDIO with LUNAR_API_URL + LUNAR_TOKEN in env.
 *   7. Drives JSON-RPC 2.0 (NDJSON framing): initialize -> tools/list ->
 *      fork_workspace -> list_workspaces -> push.
 *   8. Kills both child processes and removes temp dirs in a finally block.
 */

import { describe, it, expect } from 'vitest'
import { spawn, execFileSync, execSync } from 'node:child_process'
import {
  mkdtempSync,
  existsSync,
  writeFileSync,
  mkdirSync,
  rmSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createServer } from 'node:net'
import { createHash, randomBytes } from 'node:crypto'
import type { ChildProcess } from 'node:child_process'

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

const _dir = dirname(fileURLToPath(import.meta.url))
const PKG_ROOT = resolve(_dir, '..')
const REPO_ROOT = resolve(PKG_ROOT, '..', '..')
const LUNAR_BIN = resolve(REPO_ROOT, 'target', 'debug', 'lunar')
const MCP_ENTRY = resolve(PKG_ROOT, 'dist', 'index.js')

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function findFreePort(): Promise<number> {
  return new Promise((res, rej) => {
    const srv = createServer()
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address()
      const port = typeof addr === 'object' && addr !== null ? addr.port : 0
      srv.close(() => res(port))
    })
    srv.on('error', rej)
  })
}

async function waitForServer(url: string, maxMs: number): Promise<void> {
  const deadline = Date.now() + maxMs
  const LIMIT = 150
  for (let i = 0; i < LIMIT && Date.now() < deadline; i++) {
    try {
      const r = await fetch(url)
      if (r.status < 600) return
    } catch {
      // connection refused — not ready yet
    }
    await new Promise<void>(r => setTimeout(r, 200))
  }
  throw new Error(`server did not become ready within ${maxMs}ms at ${url}`)
}

function killProc(proc: ChildProcess): void {
  try {
    proc.kill('SIGTERM')
  } catch {
    // already dead
  }
}

/**
 * Mirror of test/multidevice/run.sh seeding.
 * Shells out to the system sqlite3 binary so we need zero npm dependencies.
 */
function seedDb(dbPath: string, tokenHashHex: string): void {
  const ts = Math.floor(Date.now() / 1000)
  const sql = [
    `INSERT INTO users(external_clerk_id, created_at) VALUES(NULL, ${ts});`,
    `INSERT INTO organizations(slug, created_at) VALUES('team', ${ts});`,
    `INSERT INTO memberships(user_id, org_id, role) VALUES(1, 1, 'owner');`,
    `INSERT INTO workspaces(name, owner_kind, owner_id, created_at) VALUES('demo', 'org', 1, ${ts});`,
    `INSERT INTO api_tokens(principal_kind, principal_id, token_hash, scope, created_at, expires_at, revoked_at) ` +
      `VALUES('user', '1', X'${tokenHashHex}', NULL, ${ts}, NULL, NULL);`,
    `INSERT INTO acl_grants(principal_kind, principal_id, workspace_id, path_prefix, permission, created_at) ` +
      `VALUES('org', '1', 1, '/', 'write', ${ts});`,
  ].join('\n')
  execFileSync('sqlite3', [dbPath], { input: sql, encoding: 'utf8' })
}

// ---------------------------------------------------------------------------
// Minimal STDIO JSON-RPC 2.0 client (NDJSON framing: one JSON object per line)
// ---------------------------------------------------------------------------

interface RpcPending {
  res: (v: unknown) => void
  rej: (e: Error) => void
}

class McpStdioClient {
  private buf = ''
  private pending = new Map<number, RpcPending>()
  private nextId = 1

  constructor(private readonly proc: ChildProcess) {
    if (!proc.stdout) throw new Error('process must have piped stdout')
    proc.stdout.setEncoding('utf8')
    proc.stdout.on('data', (chunk: string) => { this.onData(chunk) })
  }

  private onData(chunk: string): void {
    this.buf += chunk
    let nl: number
    while ((nl = this.buf.indexOf('\n')) !== -1) {
      const line = this.buf.slice(0, nl).replace(/\r$/, '')
      this.buf = this.buf.slice(nl + 1)
      if (!line.trim()) continue
      let msg: unknown
      try { msg = JSON.parse(line) } catch { continue }
      const m = msg as { id?: number; result?: unknown; error?: unknown }
      if (typeof m.id === 'number') {
        const p = this.pending.get(m.id)
        if (p) {
          this.pending.delete(m.id)
          if (m.error !== undefined) {
            p.rej(new Error(`MCP error (id=${m.id}): ${JSON.stringify(m.error)}`))
          } else {
            p.res(m.result)
          }
        }
      }
    }
  }

  request(method: string, params?: unknown): Promise<unknown> {
    const id = this.nextId++
    if (!this.proc.stdin) throw new Error('process stdin must be writable')
    const line = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n'
    this.proc.stdin.write(line)
    return new Promise<unknown>((res, rej) => {
      const timer = setTimeout(() => {
        this.pending.delete(id)
        rej(new Error(`MCP request "${method}" (id=${id}) timed out after 15s`))
      }, 15_000)
      this.pending.set(id, {
        res: (v) => { clearTimeout(timer); res(v) },
        rej: (e) => { clearTimeout(timer); rej(e) },
      })
    })
  }

  notify(method: string, params?: unknown): void {
    if (!this.proc.stdin) return
    this.proc.stdin.write(JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n')
  }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

describe('lunarfs-mcp STDIO E2E (gated on LUNAR_MCP_E2E=1)', () => {
  it(
    'drives initialize -> tools/list -> fork_workspace -> list_workspaces -> push against live lunar serve',
    { timeout: 120_000 },
    async () => {
      if (process.env['LUNAR_MCP_E2E'] !== '1') {
        // Not gated: clean skip. NEVER fail.
        return
      }

      // Verify that the lunar Rust binary exists (built by cargo build).
      if (!existsSync(LUNAR_BIN)) {
        throw new Error(
          `lunar binary not found at ${LUNAR_BIN}; run "cargo build" from the repo root first`,
        )
      }

      // Build lunarfs-mcp TypeScript bundle if the dist entry is missing.
      if (!existsSync(MCP_ENTRY)) {
        execSync('npm run build', { cwd: PKG_ROOT, stdio: 'inherit' })
      }

      // Unique temp dirs so parallel runs cannot collide.
      const tmpBase = mkdtempSync(join(tmpdir(), 'lunarfs-mcp-e2e-'))
      const storeDir = join(tmpBase, 'store')
      const dbPath = join(tmpBase, 'lunar.db')
      const contentDir = join(tmpBase, 'content')
      mkdirSync(storeDir, { recursive: true })
      mkdirSync(contentDir, { recursive: true })
      writeFileSync(join(contentDir, 'hello.txt'), 'lunar e2e test seed\n')

      const token = 'ddb_' + randomBytes(24).toString('hex')
      const tokenHashHex = createHash('sha256').update(token).digest('hex')
      const port = await findFreePort()
      const serverAddr = `127.0.0.1:${port}`
      const baseUrl = `http://${serverAddr}`

      let serveProc: ChildProcess | null = null
      let mcpProc: ChildProcess | null = null

      try {
        // ----------------------------------------------------------------
        // 1. Start lunar serve
        // ----------------------------------------------------------------
        serveProc = spawn(
          LUNAR_BIN,
          ['serve', '--store', `local:${storeDir}`, '--addr', serverAddr, '--db', dbPath],
          { stdio: ['ignore', 'pipe', 'pipe'] },
        )
        serveProc.on('error', (e: Error) => { throw e })

        // 2. Wait for real HTTP readiness (any response beats connection-refused).
        await waitForServer(`${baseUrl}/v1/workspaces`, 30_000)

        // ----------------------------------------------------------------
        // 3. Seed the identity db (mirrors test/multidevice/run.sh exactly)
        // ----------------------------------------------------------------
        seedDb(dbPath, tokenHashHex)

        // ----------------------------------------------------------------
        // 4. Ingest a seed file into the local CAS and push to "demo" so the
        //    fork endpoint has a ref to copy from.
        // ----------------------------------------------------------------
        const ingestOut = execFileSync(LUNAR_BIN, ['ingest', contentDir], {
          encoding: 'utf8',
        }).trim()
        if (!/^[0-9a-f]{64}$/.test(ingestOut)) {
          throw new Error(`unexpected output from "lunar ingest": ${ingestOut}`)
        }
        execFileSync(LUNAR_BIN, ['push', 'demo', ingestOut], {
          encoding: 'utf8',
          // LUNAR_ORG qualifies the bare workspace name "demo" to org "team".
          env: { ...process.env, LUNAR_BASE_URL: baseUrl, LUNAR_TOKEN: token, LUNAR_ORG: 'team' },
        })

        // ----------------------------------------------------------------
        // 5. Spawn lunarfs-mcp over STDIO
        // ----------------------------------------------------------------
        mcpProc = spawn(process.execPath, [MCP_ENTRY], {
          stdio: ['pipe', 'pipe', 'pipe'],
          env: {
            ...process.env,
            LUNAR_API_URL: baseUrl,
            LUNAR_TOKEN: token,
          },
        })
        mcpProc.on('error', (e: Error) => { throw e })

        const mcp = new McpStdioClient(mcpProc)

        // ----------------------------------------------------------------
        // 6. initialize
        // ----------------------------------------------------------------
        const initResult = await mcp.request('initialize', {
          protocolVersion: '2025-11-25',
          capabilities: {},
          clientInfo: { name: 'e2e-test', version: '0.0.1' },
        })
        const init = initResult as { protocolVersion?: string; serverInfo?: { name?: string } }
        expect(init.serverInfo?.name).toBe('lunarfs-mcp')

        // Required by the MCP spec: client sends initialized notification before any requests.
        mcp.notify('notifications/initialized')

        // ----------------------------------------------------------------
        // 7. tools/list: assert all 6 expected tool names are present
        // ----------------------------------------------------------------
        const listResult = await mcp.request('tools/list')
        const toolsBody = listResult as { tools?: { name: string }[] }
        const toolNames = (toolsBody.tools ?? []).map(t => t.name)
        const EXPECTED = ['fork_workspace', 'mount', 'list_workspaces', 'push', 'grant_access', 'destroy']
        for (const name of EXPECTED) {
          expect(toolNames, `expected tool "${name}" in tools/list`).toContain(name)
        }
        expect(toolNames).toHaveLength(6)

        // ----------------------------------------------------------------
        // 8. fork_workspace: fork "demo" into "demo-fork"
        // ----------------------------------------------------------------
        const forkResult = await mcp.request('tools/call', {
          name: 'fork_workspace',
          arguments: { source: 'demo', name: 'demo-fork' },
        })
        const forkCall = forkResult as {
          content?: { type: string; text: string }[]
          isError?: boolean
        }
        expect(forkCall.isError, `fork_workspace isError: ${JSON.stringify(forkCall)}`).not.toBe(true)
        const forkData = JSON.parse(forkCall.content?.[0]?.text ?? '{}') as { ok?: boolean }
        expect(forkData.ok, `fork_workspace not ok: ${JSON.stringify(forkData)}`).toBe(true)

        // ----------------------------------------------------------------
        // 9. list_workspaces: assert success (may return empty lifecycle list)
        // ----------------------------------------------------------------
        const listWsResult = await mcp.request('tools/call', {
          name: 'list_workspaces',
          arguments: {},
        })
        const listWsCall = listWsResult as {
          content?: { type: string; text: string }[]
          isError?: boolean
        }
        expect(
          listWsCall.isError,
          `list_workspaces isError: ${JSON.stringify(listWsCall)}`,
        ).not.toBe(true)
        const listWsData = JSON.parse(listWsCall.content?.[0]?.text ?? '{}') as { ok?: boolean }
        expect(listWsData.ok, `list_workspaces not ok: ${JSON.stringify(listWsData)}`).toBe(true)

        // ----------------------------------------------------------------
        // 10. push: re-push the "demo-fork" ref to validate auth + write-ACL
        // ----------------------------------------------------------------
        const pushResult = await mcp.request('tools/call', {
          name: 'push',
          arguments: { workspace: 'demo-fork' },
        })
        const pushCall = pushResult as {
          content?: { type: string; text: string }[]
          isError?: boolean
        }
        expect(pushCall.isError, `push isError: ${JSON.stringify(pushCall)}`).not.toBe(true)
        const pushData = JSON.parse(pushCall.content?.[0]?.text ?? '{}') as {
          ok?: boolean
          data?: { pushed?: boolean; revision?: string }
        }
        expect(pushData.ok, `push not ok: ${JSON.stringify(pushData)}`).toBe(true)
        expect(pushData.data?.pushed).toBe(true)
        expect(typeof pushData.data?.revision).toBe('string')
      } finally {
        if (mcpProc) killProc(mcpProc)
        if (serveProc) killProc(serveProc)
        // Brief grace period so processes release file handles before rm.
        await new Promise<void>(r => setTimeout(r, 400))
        try { rmSync(tmpBase, { recursive: true, force: true }) } catch { /* ignore */ }
      }
    },
  )
})
