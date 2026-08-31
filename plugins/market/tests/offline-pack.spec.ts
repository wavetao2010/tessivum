import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const manifest = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')) as Record<string, unknown>

describe('offline package contract', () => {
  it('bundles runtime dependencies and leaves Cordis as an optional singleton peer', () => {
    expect(manifest.bundledDependencies).toEqual(['js-yaml', 'undici'])
    expect((manifest.peerDependenciesMeta as Record<string, { optional?: boolean }>)['@deepseek-ai/cordis']).toEqual({ optional: true })
  })

})
