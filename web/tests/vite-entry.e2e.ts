import { expect, test } from 'bun:test'
import { createServer } from 'node:net'
import { join } from 'node:path'

const WEB_ROOT = join(import.meta.dir, '..')

async function freePort(): Promise<number> {
  const server = createServer()
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolve)
  })
  const address = server.address()
  if (address === null || typeof address === 'string') throw new Error('port probe returned no address')
  await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()))
  return address.port
}

async function run(command: string[]): Promise<{ code: number; stderr: string }> {
  const child = Bun.spawn(command, { cwd: WEB_ROOT, stdout: 'pipe', stderr: 'pipe' })
  const timeout = setTimeout(() => child.kill(), 10_000)
  try {
    const [code, stderr] = await Promise.all([child.exited, new Response(child.stderr).text()])
    return { code, stderr }
  } finally {
    clearTimeout(timeout)
  }
}

function expectCorrection(result: { code: number; stderr: string }): void {
  expect(result.code).not.toBe(0)
  expect(result.stderr).toContain('tessivum/web is not a standalone application')
  expect(result.stderr).toContain('cargo run -- web')
  expect(result.stderr).toContain('window.__DSH_BOOT__')
}

test('rejects the package dev alias with the native-host correction', async () => {
  expectCorrection(await run([process.execPath, 'run', 'dev']))
})

test('rejects the standalone Vite server before listening', async () => {
  const port = await freePort()
  expectCorrection(await run([join(WEB_ROOT, 'node_modules/.bin/vite'), '--host', '127.0.0.1', '--port', String(port)]))
})
