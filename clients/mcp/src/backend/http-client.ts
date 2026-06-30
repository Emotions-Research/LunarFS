import type { HttpClient, HttpResponse } from './types.js'

const DEFAULT_BASE = 'http://127.0.0.1:8787'

export class ProductionHttpClient implements HttpClient {
  private readonly base: string

  constructor() {
    this.base = process.env['LUNAR_API_URL'] ?? DEFAULT_BASE
  }

  async request(method: string, path: string, body?: unknown): Promise<HttpResponse> {
    if (!method || !path) {
      return { status: 400, body: null }
    }
    const token = process.env['LUNAR_TOKEN']
    const headers: Record<string, string> = {}
    if (token) {
      headers['Authorization'] = `Bearer ${token}`
    }
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json'
    }
    const init: RequestInit = { method, headers }
    if (body !== undefined) {
      init.body = JSON.stringify(body)
    }
    const res = await fetch(`${this.base}${path}`, init)
    let parsedBody: unknown
    try {
      parsedBody = await res.json() as unknown
    } catch {
      parsedBody = null
    }
    return { status: res.status, body: parsedBody }
  }
}
