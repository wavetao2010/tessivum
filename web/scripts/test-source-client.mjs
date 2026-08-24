import { execFileSync } from 'node:child_process'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const webRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const upstreamRoot = process.env.TESSIVUM_DEEPSEEK_SOURCE ?? resolve(webRoot, '../../upstream/deepseek-harness')
const vitest = resolve(upstreamRoot, 'node_modules/.bin/vitest')

execFileSync(vitest, ['run', '--root', upstreamRoot, 'packages/client'], {
  cwd: upstreamRoot,
  stdio: 'inherit',
})
