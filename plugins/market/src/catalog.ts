import { readFileSync } from 'node:fs'
import type { CatalogSource, Registry, RegistryPlugin, TessivumCompatibility } from './registry.ts'

interface FixedCatalog extends Registry {
  plugins: Array<RegistryPlugin & {
    catalogSource: CatalogSource
    tessivumCompatibility: TessivumCompatibility
  }>
}

export interface VerificationEntry {
  npm: string
  version: string
  repository: string
  integrity: string
  license: string
  profile: string
  runtimes: string[]
  minimumTessivum: string
  status: 'verified' | 'revoked'
  verifiedAt: string
  evidence: string
  reason?: string
}

interface VerificationLedger {
  schema: 'tessivum.plugin-verification/v1'
  entries: VerificationEntry[]
}

function fixedLedger(): VerificationEntry[] {
  const raw = readFileSync(new URL('../compatibility.json', import.meta.url), 'utf8')
  const ledger = JSON.parse(raw) as VerificationLedger
  if (ledger.schema !== 'tessivum.plugin-verification/v1' || !Array.isArray(ledger.entries)) {
    throw new Error('invalid plugin verification ledger')
  }
  const names = new Set<string>()
  for (const entry of ledger.entries) {
    if (!entry.npm || !entry.version || !entry.repository || !entry.integrity || !entry.license
      || !entry.profile || !entry.minimumTessivum || !entry.verifiedAt || !entry.evidence
      || !Array.isArray(entry.runtimes) || entry.runtimes.length === 0
      || !['verified', 'revoked'].includes(entry.status) || names.has(entry.npm)
      || (entry.status === 'revoked' && !entry.reason)) {
      throw new Error('invalid plugin verification ledger entry')
    }
    names.add(entry.npm)
  }
  return ledger.entries
}

function fixedCatalog(): FixedCatalog {
  const raw = readFileSync(new URL('../catalog.json', import.meta.url), 'utf8')
  const catalog = JSON.parse(raw) as FixedCatalog
  if (!Array.isArray(catalog.plugins)) throw new Error('invalid first-party catalog')
  for (const plugin of catalog.plugins) {
    if (plugin.catalogSource !== 'tessivum' || plugin.tessivumCompatibility !== 'official') {
      throw new Error('invalid first-party catalog metadata')
    }
  }
  return catalog
}

/** Merge release-fixed first-party entries with the live DSH community registry. */
export function marketCatalog(community: Registry, ledger: VerificationEntry[] = fixedLedger()): Registry {
  const firstParty = fixedCatalog()
  const firstPartyNames = new Set(firstParty.plugins.flatMap(plugin => [plugin.name, plugin.npm].filter((name): name is string => typeof name === 'string')))
  const verification = new Map(ledger.map(entry => [entry.npm, entry]))
  const plugins = [
    ...firstParty.plugins,
    ...community.plugins
      .filter(plugin => !firstPartyNames.has(plugin.name) && (plugin.npm === undefined || plugin.npm === null || !firstPartyNames.has(plugin.npm)))
      .map(plugin => {
        const record = typeof plugin.npm === 'string' ? verification.get(plugin.npm) : undefined
        const matches = record?.repository.toLowerCase() === plugin.url.toLowerCase()
        const verified = matches && record.status === 'verified'
        return {
          ...plugin,
          ...(verified ? { install: plugin.install.replace(plugin.npm!, `${plugin.npm}@${record.version}`) } : {}),
          catalogSource: 'dsh-community' as const,
          tessivumCompatibility: verified ? 'verified' as const : 'unverified' as const,
          ...(matches ? {
            tessivumVerifiedVersion: record.version,
            tessivumVerificationEvidence: record.evidence,
            ...(record.status === 'revoked' ? {
              tessivumVerificationRevoked: true,
              tessivumVerificationReason: record.reason,
            } : {}),
          } : {}),
        }
      }),
  ]
  return {
    ...community,
    updated: firstParty.updated,
    count: plugins.length,
    categories: { ...community.categories, ...firstParty.categories },
    plugins,
  }
}
