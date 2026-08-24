import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs'
import { join, relative, resolve, sep } from 'node:path'

export const FROZEN_DSH_VERSION = '0.1.0-rc.5'

const DSH_PREFIX = '@deepseek-ai/dsh-'
const SOURCE_EXTENSIONS = ['.ts', '.tsx', '.mts', '.cts', '.js', '.jsx', '.css', '.json']

const isFile = (path) => existsSync(path) && statSync(path).isFile()
const isDirectory = (path) => existsSync(path) && statSync(path).isDirectory()
const isDshSpecifier = (specifier) => specifier.startsWith(DSH_PREFIX)
const isBareSpecifier = (specifier) => !specifier.startsWith('.') && !specifier.startsWith('/') && !specifier.startsWith('\0')

function packageId(specifier) {
  const parts = specifier.split('/')
  return specifier.startsWith('@') ? `${parts[0]}/${parts[1]}` : parts[0]
}

function packageSubpath(specifier) {
  return specifier.slice(packageId(specifier).length).replace(/^\//, '')
}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function assertInside(root, path, label) {
  const pathFromRoot = relative(root, path)
  if (pathFromRoot === '..' || pathFromRoot.startsWith(`..${sep}`)) {
    throw new Error(`DeepSeek source resolver: ${label} escapes ${root}`)
  }
  return path
}

function selectExport(value) {
  if (typeof value === 'string') return value
  if (Array.isArray(value)) {
    for (const candidate of value) {
      const selected = selectExport(candidate)
      if (selected !== undefined) return selected
    }
    return undefined
  }
  if (value === null || typeof value !== 'object') return undefined
  for (const condition of ['browser', 'import', 'default']) {
    if (condition in value) {
      const selected = selectExport(value[condition])
      if (selected !== undefined) return selected
    }
  }
  return undefined
}

function exportedPath(exports, subpath) {
  const key = subpath === '' ? '.' : `./${subpath}`
  if (typeof exports === 'string' || Array.isArray(exports)) return key === '.' ? selectExport(exports) : undefined
  if (exports === null || typeof exports !== 'object') return undefined
  if (key in exports) return selectExport(exports[key])

  for (const [pattern, target] of Object.entries(exports)) {
    const wildcard = pattern.indexOf('*')
    if (wildcard === -1) continue
    const prefix = pattern.slice(0, wildcard)
    const suffix = pattern.slice(wildcard + 1)
    if (!key.startsWith(prefix) || !key.endsWith(suffix)) continue
    const matched = key.slice(prefix.length, key.length - suffix.length)
    const selected = selectExport(target)
    if (selected !== undefined) return selected.replace('*', matched)
  }
}

function sourceFile(packageRoot, target) {
  if (typeof target !== 'string' || !target.startsWith('./')) return undefined
  const output = target.slice(2)
  const sourceRelative = output.startsWith('src/')
    ? output
    : output.startsWith('lib/types/')
      ? `src/${output.slice('lib/types/'.length)}`
      : output.startsWith('lib/')
        ? `src/${output.slice('lib/'.length)}`
        : output
  const direct = assertInside(packageRoot, resolve(packageRoot, sourceRelative), 'export')
  const stem = direct.replace(/\.(?:[cm]?js|d\.ts)$/, '')
  const candidates = [direct, ...SOURCE_EXTENSIONS.map(extension => `${stem}${extension}`)]
  for (const candidate of candidates) {
    if (isFile(candidate)) return candidate
  }
  for (const extension of SOURCE_EXTENSIONS) {
    const candidate = join(stem, `index${extension}`)
    if (isFile(candidate)) return candidate
  }
}

function dshWorkspacePackages(upstreamRoot) {
  const packagesRoot = join(upstreamRoot, 'packages')
  if (!isDirectory(packagesRoot)) {
    throw new Error(`DeepSeek source resolver: missing workspace packages at ${packagesRoot}`)
  }
  const packages = new Map()
  for (const group of readdirSync(packagesRoot)) {
    const groupRoot = join(packagesRoot, group)
    if (!isDirectory(groupRoot)) continue
    for (const leaf of readdirSync(groupRoot)) {
      const packageRoot = join(groupRoot, leaf)
      const manifestPath = join(packageRoot, 'package.json')
      if (!isFile(manifestPath)) continue
      const manifest = readJson(manifestPath)
      if (typeof manifest.name !== 'string' || !manifest.name.startsWith(DSH_PREFIX)) continue
      if (packages.has(manifest.name)) {
        throw new Error(`DeepSeek source resolver: duplicate workspace package ${manifest.name}`)
      }
      packages.set(manifest.name, { manifest, root: packageRoot })
    }
  }
  return packages
}

export function createDeepSeekSourceResolver(upstreamRoot) {
  const root = resolve(upstreamRoot)
  const rootManifestPath = join(root, 'package.json')
  if (!isFile(rootManifestPath)) {
    throw new Error(`DeepSeek source resolver: missing upstream root manifest at ${rootManifestPath}`)
  }
  const rootManifest = readJson(rootManifestPath)
  if (rootManifest.version !== FROZEN_DSH_VERSION) {
    throw new Error(
      `DeepSeek source resolver: expected upstream ${FROZEN_DSH_VERSION}, received ${String(rootManifest.version)}`,
    )
  }
  const packages = dshWorkspacePackages(root)

  function resolveDshImport(specifier) {
    if (!isDshSpecifier(specifier)) return undefined
    const name = packageId(specifier)
    const workspace = packages.get(name)
    if (workspace === undefined) {
      throw new Error(`DeepSeek source resolver: unresolved frozen package ${specifier}`)
    }
    const target = exportedPath(workspace.manifest.exports, packageSubpath(specifier))
    if (target === undefined) {
      throw new Error(`DeepSeek source resolver: ${specifier} is not exported by ${name}`)
    }
    const source = sourceFile(workspace.root, target)
    if (source === undefined) {
      throw new Error(`DeepSeek source resolver: ${specifier} export ${target} has no source file`)
    }
    return source
  }

  return {
    packages,
    root,
    resolveDshImport,
  }
}

export function deepSeekSourcePlugin(sourceResolver, resolveExternal) {
  return {
    name: 'tessivum-deepseek-source',
    enforce: 'pre',
    resolveId(specifier, importer) {
      const source = sourceResolver.resolveDshImport(specifier)
      if (source !== undefined) return source
      if (importer === undefined || !importer.startsWith(sourceResolver.root)) return undefined
      if (!isBareSpecifier(specifier) || specifier === 'node:module') return undefined
      return resolveExternal(specifier)
    },
  }
}

function localSourceFile(from, specifier) {
  const direct = resolve(from, specifier)
  if (isFile(direct)) return direct
  for (const extension of SOURCE_EXTENSIONS) {
    if (isFile(`${direct}${extension}`)) return `${direct}${extension}`
  }
  for (const extension of SOURCE_EXTENSIONS) {
    const index = join(direct, `index${extension}`)
    if (isFile(index)) return index
  }
  throw new Error(`DeepSeek source resolver: unresolved local source ${specifier} from ${from}`)
}

function importsFrom(source) {
  const imports = []
  const add = (specifier, typeOnly = false) => imports.push({ specifier, typeOnly })
  for (const match of source.matchAll(/\bimport\s+(type\s+)?(?:[\s\S]*?\sfrom\s*)?['"]([^'"]+)['"]/g)) {
    add(match[2], match[1] !== undefined)
  }
  for (const match of source.matchAll(/\bexport\s+(type\s+)?[\s\S]*?\sfrom\s*['"]([^'"]+)['"]/g)) {
    add(match[2], match[1] !== undefined)
  }
  for (const match of source.matchAll(/\bimport\s*\(\s*['"]([^'"]+)['"]\s*\)/g)) add(match[1])
  for (const match of source.matchAll(/@import\s+(?:url\()?\s*['"]([^'"]+)['"]/g)) add(match[1])
  return imports
}

export function auditSourceGraph(sourceResolver, entry = '@deepseek-ai/dsh-client-web') {
  const modules = new Set()
  const runtimePackages = new Set()
  const resolvedDsh = new Map()
  const externalPackages = new Set()

  function visit(path) {
    if (modules.has(path)) return
    modules.add(path)
    for (const { specifier, typeOnly } of importsFrom(readFileSync(path, 'utf8'))) {
      if (isDshSpecifier(specifier)) {
        const target = sourceResolver.resolveDshImport(specifier)
        resolvedDsh.set(specifier, target)
        if (!typeOnly) {
          runtimePackages.add(packageId(specifier))
          visit(target)
        }
        continue
      }
      if (specifier.startsWith('.')) {
        if (!typeOnly) visit(localSourceFile(resolve(path, '..'), specifier))
        continue
      }
      if (isBareSpecifier(specifier) && !specifier.startsWith('node:') && !typeOnly) {
        externalPackages.add(packageId(specifier))
      }
    }
  }

  const entrySource = sourceResolver.resolveDshImport(entry)
  resolvedDsh.set(entry, entrySource)
  runtimePackages.add(packageId(entry))
  visit(entrySource)
  return {
    externalPackages: [...externalPackages].sort(),
    modules: [...modules].sort(),
    packages: [...runtimePackages].sort(),
    resolvedDsh: [...resolvedDsh.entries()].sort(([left], [right]) => left.localeCompare(right)),
  }
}
