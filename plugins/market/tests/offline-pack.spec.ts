import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const manifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')) as Record<string, unknown>
const offlineFixture = JSON.parse(readFileSync(join(root, 'tests/fixtures/offline-install/package.json'), 'utf8')) as {
  dependencies: Record<string, string>
}

describe('offline package contract', () => {
  it('bundles runtime dependencies and leaves Cordis as an optional singleton peer', () => {
    expect(manifest.bundledDependencies).toEqual(['js-yaml', 'undici'])
    expect((manifest.peerDependenciesMeta as Record<string, { optional?: boolean }>)['@deepseek-ai/cordis']).toEqual({ optional: true })
  })

  it('installs the packed first-party archive without a registry specifier', () => {
    expect(offlineFixture.dependencies['tessivum-market']).toBe('file:./tessivum-market-0.1.0-alpha.17.tgz')
  })
})
