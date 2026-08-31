import { describe, expect, it } from 'vitest'
import { marketCatalog } from '../src/catalog.ts'
import type { Registry, RegistryPlugin } from '../src/registry.ts'

function plugin(name: string, npm: string | null, tessivumCompatibility?: RegistryPlugin['tessivumCompatibility']): RegistryPlugin {
  return {
    name,
    owner: 'community',
    url: `https://example.test/${name}`,
    category: 'tool',
    description: { en: name },
    npm,
    install: `dsh plugin --profile web add ${npm ?? name}`,
    added: '2026-08-31',
    tessivumCompatibility,
  }
}

describe('first-party catalog overlay', () => {
  it('keeps tested entries authoritative without duplicating community rows', () => {
    const community: Registry = {
      updated: 'live',
      count: 4,
      categories: { tool: { en: 'Tools' } },
      plugins: [
        plugin('dsh-dream-skin', 'dsh-dream-skin'),
        plugin('renamed-sidebar', 'dsh-better-sidebar'),
        plugin('verified-tool', 'verified-tool', 'verified'),
        plugin('unverified-tool', 'unverified-tool'),
      ],
    }

    const result = marketCatalog(community)
    expect(result.plugins.map(entry => entry.npm)).toEqual([
      'dsh-better-sidebar',
      'dsh-dream-skin',
      'verified-tool',
      'unverified-tool',
    ])
    expect(result.plugins.map(entry => [entry.catalogSource, entry.tessivumCompatibility])).toEqual([
      ['tessivum', 'official'],
      ['tessivum', 'official'],
      ['dsh-community', 'verified'],
      ['dsh-community', 'unverified'],
    ])
    expect(result.count).toBe(result.plugins.length)
  })
})
