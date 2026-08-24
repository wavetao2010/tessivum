import { execFileSync } from 'node:child_process'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const patch = resolve(dirname(fileURLToPath(import.meta.url)), '../patches/deepseek-harness.patch')

function gitApply(root, ...args) {
  execFileSync('git', ['apply', ...args, patch], { cwd: root, stdio: 'ignore' })
}

/** Apply Tessivum's pinned client-compatibility delta exactly once. */
export function applyDeepSeekPatch(upstreamRoot) {
  try {
    gitApply(upstreamRoot, '--reverse', '--check')
    return
  } catch {}

  gitApply(upstreamRoot, '--check')
  gitApply(upstreamRoot)
}
