import { execFile } from 'node:child_process'
import { promisify } from 'node:util'
import type { CliResult, CliRunner } from './types.js'

const execFileAsync = promisify(execFile)

export class ProductionCliRunner implements CliRunner {
  async run(args: string[]): Promise<CliResult> {
    if (args.length === 0) {
      return { stdout: '', stderr: 'no args provided', exitCode: 1 }
    }
    try {
      const { stdout, stderr } = await execFileAsync('lunar', args)
      return { stdout, stderr, exitCode: 0 }
    } catch (err) {
      const e = err as { stdout?: string; stderr?: string; code?: number }
      return {
        stdout: e.stdout ?? '',
        stderr: e.stderr ?? '',
        exitCode: typeof e.code === 'number' ? e.code : 1,
      }
    }
  }
}
