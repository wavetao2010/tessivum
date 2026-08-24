import { expect, test } from 'bun:test'
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process'
import { readFile, readdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { CRATE_ROOT, RustWebHarness, UPSTREAM_ROOT, waitUntil } from './support'

const TARGET = '@deepseek-ai/dsh-client-ui-conversation'

async function liveClientRoots(): Promise<string[]> {
  const roots: string[] = []
  const visit = async (directory: string): Promise<void> => {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name)
      if (entry.isDirectory()) await visit(path)
      else if (entry.name === 'package.json') {
        const manifest = JSON.parse(await readFile(path, 'utf8')) as { name?: string }
        roots.push(manifest.name === TARGET
          ? join(UPSTREAM_ROOT, 'packages/client/ui-conversation')
          : directory)
      }
    }
  }
  await visit(join(CRATE_ROOT, 'web/client-packages'))
  if (roots.length !== 38 || !roots.includes(join(UPSTREAM_ROOT, 'packages/client/ui-conversation'))) {
    throw new Error(`expected the pinned 38-package graph with live ${TARGET}, received ${roots.length}`)
  }
  return roots
}

function waitForWatcher(watcher: ChildProcessWithoutNullStreams): Promise<void> {
  const { promise, resolve, reject } = Promise.withResolvers<void>()
  let output = ''
  let settled = false
  const cleanup = (): void => {
    clearTimeout(timer)
    watcher.stdout.off('data', onData)
    watcher.stderr.off('data', onData)
    watcher.off('error', onError)
    watcher.off('exit', onExit)
  }
  const finish = (error?: Error): void => {
    if (settled) return
    settled = true
    cleanup()
    if (error === undefined) resolve()
    else reject(error)
  }
  const onData = (chunk: Buffer): void => {
    output += chunk.toString()
    if (output.includes('dev-web: watching')) finish()
  }
  const onError = (error: Error): void => { finish(error) }
  const onExit = (code: number | null): void => {
    finish(new Error(`dev:web exited before ready (${String(code)}):\n${output}`))
  }
  const timer = setTimeout(() => {
    finish(new Error(`dev:web did not become ready:\n${output}`))
  }, 60_000)
  watcher.stdout.on('data', onData)
  watcher.stderr.on('data', onData)
  watcher.once('error', onError)
  watcher.once('exit', onExit)
  return promise
}

async function stopWatcher(watcher: ChildProcessWithoutNullStreams): Promise<void> {
  let exited = Promise.resolve()
  if (watcher.exitCode === null && watcher.signalCode === null) {
    const completion = Promise.withResolvers<void>()
    watcher.once('exit', () => { completion.resolve() })
    exited = completion.promise
  }
  const signalTree = (signal: NodeJS.Signals): void => {
    if (watcher.pid === undefined) return
    try {
      if (process.platform === 'win32') watcher.kill(signal)
      else process.kill(-watcher.pid, signal)
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'ESRCH') throw error
    }
  }
  if (watcher.exitCode === null && watcher.signalCode === null) signalTree('SIGTERM')
  await Promise.race([exited, Bun.sleep(15_000)])
  if (watcher.exitCode === null && watcher.signalCode === null) {
    signalTree('SIGKILL')
    await exited
  }
}

test('hot-reloads a real client source edit without refreshing the page', async () => {
  const packageRoot = join(UPSTREAM_ROOT, 'packages/client/ui-conversation')
  const sourcePath = join(packageRoot, 'src/client/locales.ts')
  const bundlePath = join(packageRoot, 'lib/client.js')
  const originalSource = await readFile(sourcePath, 'utf8')
  const originalBundle = await readFile(bundlePath)
  const oldText = 'Into the Unknown'
  const sourceNeedle = "'hero.headline': 'Into the Unknown'"
  const newText = `HMR UPDATED ${'x'.repeat(80)}`
  const updatedSource = originalSource.replace(sourceNeedle, `'hero.headline': '${newText}'`)
  if (updatedSource === originalSource) throw new Error(`HMR source lacks ${JSON.stringify(sourceNeedle)}`)

  const watcher = spawn('npm', ['run', 'dev:web'], {
    cwd: UPSTREAM_ROOT,
    detached: process.platform !== 'win32',
    stdio: 'pipe',
  })
  const failures: unknown[] = []
  try {
    await waitForWatcher(watcher)
    const harness = await RustWebHarness.launch({
      name: 'hmr-live',
      clientPackageRoots: await liveClientRoots(),
    })
    try {
      await harness.page.getByText(oldText, { exact: true }).waitFor({ timeout: 15_000 })
      const identity = await harness.page.evaluate(() => {
        const value = crypto.randomUUID()
        Object.defineProperty(window, '__tessivumHmrPageIdentity', { value })
        return value
      })
      await writeFile(sourcePath, updatedSource)
      await waitUntil(() => harness.page.getByText(newText, { exact: true }).count(), count => count > 0, 30_000)
      expect(await harness.page.evaluate(() => (window as Window & { __tessivumHmrPageIdentity?: string }).__tessivumHmrPageIdentity)).toBe(identity)
      expect(harness.pageErrors).toEqual([])
    } finally {
      await harness.close()
    }
  } catch (error) {
    failures.push(error)
  } finally {
    await writeFile(sourcePath, originalSource).catch(error => { failures.push(error) })
    await stopWatcher(watcher).catch(error => { failures.push(error) })
    await writeFile(bundlePath, originalBundle).catch(error => { failures.push(error) })
  }
  if (failures.length > 0) throw new AggregateError(failures, 'HMR browser contract or cleanup failed')
}, 240_000)
