import { describe, expect, it } from 'vitest'
import { marketCatalog, type VerificationEntry, type VerificationLedger } from '../src/catalog.ts'
import type { Registry, RegistryPlugin } from '../src/registry.ts'

function plugin(name: string, npm: string | null, tessivumCompatibility?: RegistryPlugin['tessivumCompatibility'], url = `https://example.test/${name}`): RegistryPlugin {
  return {
    name,
    owner: 'community',
    url,
    category: 'tool',
    description: { en: name },
    npm,
    install: `dsh plugin --profile web add ${npm ?? name}`,
    added: '2026-08-31',
    tessivumCompatibility,
  }
}

function verification(status: VerificationEntry['status'], repository = 'https://github.com/omdsh-dev/DSH-better-sidebar'): VerificationEntry {
  return {
    npm: 'dsh-better-sidebar',
    version: '0.16.1',
    repository,
    integrity: 'sha512-test',
    license: 'MIT',
    profile: 'web',
    runtimes: ['legacy-node', 'browser'],
    minimumTessivum: '0.1.0-alpha.23',
    verification: {
      browserBootEntry: 'dsh-better-sidebar',
      browserFeature: 'sidebar-panel',
      browserFeatureSelector: '[data-dsh-panel]',
      updateVersion: '0.17.1',
      failureVersion: '99.99.99',
    },
    status,
    verifiedAt: '2026-09-03',
    evidence: 'docs/PLUGIN_VERIFICATION_REPORT.md',
    sha256: '0'.repeat(64),
    ...(status === 'revoked' ? { reason: 'fixture revocation' } : {}),
  }
}

function ledger(entries: VerificationEntry[], current: Record<string, string> = {}): VerificationLedger {
  return { schema: 'tessivum.plugin-verification/v2', current, entries }
}

describe('first-party catalog overlay', () => {
  it('keeps tested entries authoritative without duplicating community rows', () => {
    const community: Registry = {
      updated: 'live',
      count: 4,
      categories: { tool: { en: 'Tools' } },
      plugins: [
        plugin('dsh-dream-skin', 'dsh-dream-skin'),
        plugin('dsh-better-sidebar', 'dsh-better-sidebar', undefined, 'https://github.com/omdsh-dev/DSH-better-sidebar'),
        plugin('spoofed-verification', 'spoofed-verification', 'verified'),
        plugin('unverified-tool', 'unverified-tool'),
      ],
    }

    const result = marketCatalog(community)
    expect(result.plugins.map(entry => entry.npm)).toEqual([
      'tessivum-market',
      'dsh-dream-skin',
      'dsh-better-sidebar',
      'spoofed-verification',
      'unverified-tool',
    ])
    expect(result.plugins.map(entry => [entry.catalogSource, entry.tessivumCompatibility])).toEqual([
      ['tessivum', 'official'],
      ['dsh-community', 'unverified'],
      ['dsh-community', 'verified'],
      ['dsh-community', 'unverified'],
      ['dsh-community', 'unverified'],
    ])
    const verified = result.plugins[2]!
    expect(verified.tessivumVerifiedVersion).toBe('0.16.1')
    expect(verified.install).toBe('dsh plugin --profile web add dsh-better-sidebar@0.16.1')
    expect(result.count).toBe(result.plugins.length)
  })

  it('downgrades mismatched and revoked exact releases', () => {
    const community: Registry = {
      updated: 'live',
      count: 1,
      categories: { tool: { en: 'Tools' } },
      plugins: [plugin('dsh-better-sidebar', 'dsh-better-sidebar', undefined, 'https://github.com/omdsh-dev/DSH-better-sidebar')],
    }

    const mismatched = marketCatalog(community, ledger([verification('verified', 'https://github.com/other/repo')], { 'dsh-better-sidebar': '0.16.1' })).plugins.at(-1)!
    expect(mismatched.tessivumCompatibility).toBe('unverified')
    expect(mismatched.tessivumVerifiedVersion).toBeUndefined()

    const revoked = marketCatalog(community, ledger([verification('revoked')])).plugins.at(-1)!
    expect(revoked.tessivumCompatibility).toBe('unverified')
    expect(revoked.tessivumVerifiedVersion).toBe('0.16.1')
    expect(revoked.tessivumVerificationRevoked).toBe(true)
    expect(revoked.tessivumVerificationReason).toBe('fixture revocation')
    expect(revoked.install).toBe('dsh plugin --profile web add dsh-better-sidebar')
  })

  it('selects one current release while retaining exact-version history', () => {
    const community: Registry = {
      updated: 'live',
      count: 1,
      categories: { tool: { en: 'Tools' } },
      plugins: [plugin('dsh-better-sidebar', 'dsh-better-sidebar', undefined, 'https://github.com/omdsh-dev/DSH-better-sidebar')],
    }
    const old = verification('revoked')
    const current = { ...verification('verified'), version: '0.17.1' }
    const selected = marketCatalog(community, ledger([old, current], { 'dsh-better-sidebar': '0.17.1' })).plugins.at(-1)!
    expect(selected.tessivumVerifiedVersion).toBe('0.17.1')
    expect(selected.tessivumVerificationRevoked).toBeUndefined()
    expect(selected.install).toBe('dsh plugin --profile web add dsh-better-sidebar@0.17.1')
    expect(() => marketCatalog(community, ledger([old, current], { 'dsh-better-sidebar': '0.16.1' }))).toThrow(/current plugin verification selection/)
    expect(() => marketCatalog(community, ledger([current, current], { 'dsh-better-sidebar': '0.17.1' }))).toThrow(/ledger entry/)
  })
})
