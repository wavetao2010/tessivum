import { execFileSync } from 'node:child_process'
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createDeepSeekSourceResolver } from './deepseek-source-resolver.mjs'

const webRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const upstreamRoot = process.env.TESSIVUM_DEEPSEEK_SOURCE ?? resolve(webRoot, '../../upstream/deepseek-harness')
const source = createDeepSeekSourceResolver(upstreamRoot)
const clientPackage = source.packages.get('@deepseek-ai/dsh-client-web')
if (clientPackage === undefined || typeof clientPackage.manifest.types !== 'string') {
  throw new Error('DeepSeek client Web package has no declarations')
}
const clientEntry = resolve(clientPackage.root, clientPackage.manifest.types)
const temporary = mkdtempSync(join(tmpdir(), 'tessivum-web-tsconfig-'))
const config = join(temporary, 'tsconfig.json')

writeFileSync(config, JSON.stringify({
  extends: resolve(webRoot, 'tsconfig.json'),
  compilerOptions: {
    paths: { '@deepseek-ai/dsh-client-web': [clientEntry] },
  },
  include: [resolve(webRoot, 'src')],
}))
try {
  execFileSync(resolve(webRoot, 'node_modules/.bin/tsc'), ['--project', config], { cwd: webRoot, stdio: 'inherit' })
} finally {
  rmSync(temporary, { force: true, recursive: true })
}
