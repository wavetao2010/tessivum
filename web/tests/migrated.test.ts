import { Glob } from 'bun'
import { test } from 'bun:test'

const patterns = process.env.TESSIVUM_TEST_FILES?.split(',') ?? ['*.e2e.ts', '*.snapshot.ts']

test('migrated web suite', async () => {
  const files = new Set<string>()
  for (const pattern of patterns) {
    for await (const file of new Glob(pattern).scan({ cwd: import.meta.dir, absolute: true })) files.add(file)
  }

  const failed: string[] = []
  for (const file of [...files].sort()) {
    const child = Bun.spawn([process.execPath, 'test', file, '--timeout', '1200000'], {
      cwd: import.meta.dir,
      env: process.env,
      stdout: 'inherit',
      stderr: 'inherit',
    })
    if (await child.exited !== 0) failed.push(file)
  }
  if (failed.length !== 0) throw new Error(`migrated web tests failed:\n${failed.join('\n')}`)
}, 3_600_000)
