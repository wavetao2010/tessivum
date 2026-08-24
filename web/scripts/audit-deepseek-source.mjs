import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { auditSourceGraph, createDeepSeekSourceResolver } from './deepseek-source-resolver.mjs'
import { applyDeepSeekPatch } from './prepare-deepseek-source.mjs'

const webRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const upstreamRoot = process.env.TESSIVUM_DEEPSEEK_SOURCE ?? resolve(webRoot, '../../upstream/deepseek-harness')
applyDeepSeekPatch(upstreamRoot)
const packageManifest = JSON.parse(readFileSync(resolve(webRoot, 'package.json'), 'utf8'))
const lock = readFileSync(resolve(webRoot, 'bun.lock'), 'utf8')
const sourceResolver = createDeepSeekSourceResolver(upstreamRoot)
const graph = auditSourceGraph(sourceResolver)

const fail = (message) => {
  throw new Error(`DeepSeek source audit: ${message}`)
}

const dshDependencies = Object.keys(packageManifest.dependencies).filter(name => name.startsWith('@deepseek-ai/dsh-'))
if (dshDependencies.length > 0) {
  fail(`published DSH dependencies remain: ${dshDependencies.join(', ')}`)
}
if (/"@deepseek-ai\/dsh-[^"]+": \["@deepseek-ai\/dsh-/.test(lock)) {
  fail('bun.lock retains published DSH artifacts')
}

const missingDependencies = graph.externalPackages.filter(name => !(name in packageManifest.dependencies))
if (missingDependencies.length > 0) {
  fail(`source imports are not direct dependencies: ${missingDependencies.join(', ')}`)
}

const workspace = lock.slice(0, lock.indexOf('\n  "packages":'))
const lockDependencies = workspace.match(/"dependencies": \{\n([\s\S]*?)\n\s*\},\n\s*"devDependencies"/)?.[1]
if (lockDependencies === undefined) fail('workspace dependency metadata is missing')
const locked = Object.fromEntries(
  [...lockDependencies.matchAll(/^\s+"([^"]+)": "([^"]+)",?$/gm)].map(([, name, version]) => [name, version]),
)

const declared = packageManifest.dependencies
for (const [name, version] of Object.entries(declared)) {
  if (locked[name] !== version) fail(`bun.lock does not pin ${name}@${version}`)
  const escapedName = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const escapedVersion = version.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  if (!new RegExp(`"${escapedName}": \\["${escapedName}@${escapedVersion}"`).test(lock)) {
    fail(`bun.lock has no resolved package for ${name}@${version}`)
  }
}
for (const name of Object.keys(locked)) {
  if (!(name in declared)) fail(`bun.lock retains undeclared workspace dependency ${name}`)
}

for (const [specifier, source] of graph.resolvedDsh) {
  if (!source.startsWith(sourceResolver.root) || source.includes('/node_modules/')) {
    fail(`${specifier} did not resolve to frozen source`)
  }
}

console.log(
  `DeepSeek source audit OK: ${graph.packages.length} packages, ${graph.modules.length} modules, ${graph.resolvedDsh.length} DSH exports`,
)
