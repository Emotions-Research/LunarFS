import { timingSafeEqual } from 'node:crypto'

export type TransportMode = 'stdio' | 'remote'

export interface RemoteConfig {
  host: string
  port: number
  token: string
}

export interface ResolvedConfig {
  mode: TransportMode
  remote?: RemoteConfig
}

function isTruthy(value: string | undefined): boolean {
  if (value === undefined) return false
  return ['1', 'true', 'yes'].includes(value.toLowerCase().trim())
}

function parsePort(value: string | undefined): number {
  if (value === undefined) return 3000
  const n = Number.parseInt(value, 10)
  return Number.isFinite(n) && n > 0 ? n : 3000
}

export function resolveConfig(env: Record<string, string | undefined> = process.env): ResolvedConfig {
  const flag = isTruthy(env['LUNAR_MCP_REMOTE'])
  if (!flag) return { mode: 'stdio' }

  const token = env['LUNAR_MCP_TOKEN']?.trim()
  if (!token) throw new Error('LUNAR_MCP_REMOTE is set but LUNAR_MCP_TOKEN is empty')

  return {
    mode: 'remote',
    remote: {
      host: env['LUNAR_MCP_HOST']?.trim() || '127.0.0.1',
      port: parsePort(env['LUNAR_MCP_PORT']),
      token,
    },
  }
}

// Compares the Bearer token with constant-time equality to resist timing attacks.
// Case-sensitive prefix match is intentional: 'Bearer' is the registered scheme (RFC 6750).
export function isAuthorized(authHeader: string | undefined, expectedToken: string): boolean {
  if (authHeader === undefined) return false
  if (!authHeader.startsWith('Bearer ')) return false
  const presented = authHeader.slice('Bearer '.length)
  const a = Buffer.from(presented, 'utf8')
  const b = Buffer.from(expectedToken, 'utf8')
  if (a.length !== b.length) return false
  return timingSafeEqual(a, b)
}
