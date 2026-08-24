import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createDeepSeekSourceResolver, FROZEN_DSH_VERSION } from './deepseek-source-resolver.mjs'
import { applyDeepSeekPatch } from './prepare-deepseek-source.mjs'

const webRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const upstreamRoot = process.env.TESSIVUM_DEEPSEEK_SOURCE ?? resolve(webRoot, '../../upstream/deepseek-harness')
applyDeepSeekPatch(upstreamRoot)
const outputRoot = resolve(webRoot, 'client-packages')
const source = createDeepSeekSourceResolver(upstreamRoot)

execFileSync('npm', ['run', 'build:lib:client'], { cwd: source.root, stdio: 'inherit' })

const profile = readFileSync(join(source.root, 'packages/bundle/web-app/cordis.patch.yml'), 'utf8')
const selected = new Set(
  [...profile.matchAll(/^\s*name:\s*'([^']+)'\s*$/gm)]
    .map(([, name]) => name)
    .filter(name => source.packages.get(name)?.manifest.dsh?.client?.platform === 'web'),
)
// Tessivum exposes the in-page directory browser, not a process-native chooser.
selected.add('@deepseek-ai/dsh-client-ui-directory-picker-browse')

for (const name of selected) {
  const workspace = source.packages.get(name)
  if (workspace === undefined) throw new Error(`client bundle source package is missing: ${name}`)
  for (const dependency of workspace.manifest.dsh.client.inject ?? []) {
    if (source.packages.get(dependency)?.manifest.dsh?.client?.platform === 'web') selected.add(dependency)
  }
}
if (selected.size !== 38) throw new Error(`expected 38 source client packages, received ${selected.size}`)

rmSync(outputRoot, { force: true, recursive: true })
const entries = []
for (const id of [...selected].sort()) {
  const { manifest, root } = source.packages.get(id)
  if (manifest.version !== FROZEN_DSH_VERSION) {
    throw new Error(`${id} has version ${String(manifest.version)}, expected ${FROZEN_DSH_VERSION}`)
  }
  const clientExport = manifest.exports?.['./client']
  const target = typeof clientExport === 'string' ? clientExport : clientExport?.default
  if (typeof target !== 'string' || !target.startsWith('./')) {
    throw new Error(`${id} has no default ./client export`)
  }
  const bundle = resolve(root, target)
  const bytes = readFileSync(bundle)
  const text = bytes.toString('utf8')
  const handoff = new RegExp(`id:\\s*["']${id.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}["']`)
  if (!text.includes('window.__ModuleLoader__.load({') || !handoff.test(text)) {
    throw new Error(`${id} bundle does not register its package id`)
  }

  const destination = join(outputRoot, relative(join(source.root, 'packages'), root))
  mkdirSync(join(destination, dirname(target)), { recursive: true })
  copyFileSync(join(root, 'package.json'), join(destination, 'package.json'))
  copyFileSync(bundle, join(destination, target))
  const staged = readFileSync(join(destination, target))
  if (!bytes.equals(staged)) throw new Error(`${id} staged bundle differs from source build`)
  if (existsSync(`${bundle}.map`)) copyFileSync(`${bundle}.map`, join(destination, `${target}.map`))

  const rev = createHash('sha1').update(bytes).digest('hex').slice(0, 12)
  const client = manifest.dsh.client
  entries.push({
    id,
    url: `/plugins/${id}/client.js?rev=${rev}`,
    rev,
    ...(client.inject === undefined ? {} : { inject: client.inject }),
    ...(client.immediately === undefined ? {} : { immediately: client.immediately }),
  })
}
const graphRev = createHash('sha1').update(JSON.stringify(entries)).digest('hex').slice(0, 12)
writeFileSync(join(outputRoot, 'bundles.json'), `${JSON.stringify({ version: FROZEN_DSH_VERSION, graphRev, entries }, null, 2)}\n`)
console.log(`DeepSeek source client bundles OK: ${entries.length} packages, graph ${graphRev}`)
