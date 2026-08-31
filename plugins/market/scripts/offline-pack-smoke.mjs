import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { copyFileSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const manifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'))
const filename = `${manifest.name}-${manifest.version}.tgz`
const temporary = mkdtempSync(join(tmpdir(), 'tessivum-market-offline-'))
const environment = { ...process.env, BUN_INSTALL_CACHE_DIR: join(temporary, 'empty-cache') }

function run(args, cwd) {
  try {
    return execFileSync('bun', args, { cwd, env: environment, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] })
  } catch (error) {
    process.stderr.write(error.stdout ?? '')
    process.stderr.write(error.stderr ?? '')
    throw error
  }
}

function digest(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex')
}

try {
  const first = join(temporary, 'first')
  const second = join(temporary, 'second')
  mkdirSync(first)
  mkdirSync(second)
  for (const destination of [first, second]) {
    run(['pm', 'pack', '--ignore-scripts', '--destination', destination], root)
  }
  const firstArchive = join(first, filename)
  const secondArchive = join(second, filename)
  if (digest(firstArchive) !== digest(secondArchive)) throw new Error('market package is not reproducible')

  const profile = join(temporary, 'profile')
  mkdirSync(profile)
  copyFileSync(firstArchive, join(profile, filename))
  writeFileSync(join(profile, 'package.json'), JSON.stringify({
    name: 'tessivum-market-offline-smoke',
    private: true,
    type: 'module',
    dependencies: { [manifest.name]: `file:./${filename}` },
  }))
  run(['install', '--offline', '--ignore-scripts'], profile)
  run(['-e', `await import(${JSON.stringify(pathToFileURL(join(profile, 'node_modules', manifest.name, manifest.main)).href)})`], profile)
  process.stdout.write('offline market pack/install smoke passed\n')
} finally {
  rmSync(temporary, { recursive: true, force: true })
}
