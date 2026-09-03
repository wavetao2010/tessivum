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
  verification: {
    browserBootEntry: string
    updateVersion: string
    failureVersion: string
  }
  status: 'verified' | 'revoked'
  verifiedAt: string
  evidence: string
  reason?: string
}

export interface VerificationLedger {
  schema: 'tessivum.plugin-verification/v2'
  current: Record<string, string>
  entries: VerificationEntry[]
}

function validateLedger(ledger: VerificationLedger): VerificationLedger {
  if (ledger.schema !== 'tessivum.plugin-verification/v2' || !ledger.current || typeof ledger.current !== 'object' || !Array.isArray(ledger.entries)) {
    throw new Error('invalid plugin verification ledger')
  }
  const pairs = new Set<string>()
  const entries = new Map<string, VerificationEntry>()
  const verifiedPackages = new Set<string>()
  for (const entry of ledger.entries) {
    const pair = `${entry.npm}@${entry.version}`
    if (!entry.npm || !entry.version || !entry.repository || !entry.integrity || !entry.license
      || !entry.profile || !entry.minimumTessivum || !entry.verifiedAt || !entry.evidence
      || !entry.verification || entry.verification.browserBootEntry !== entry.npm
      || !entry.verification.updateVersion || !entry.verification.failureVersion
      || !Array.isArray(entry.runtimes) || entry.runtimes.length === 0
      || !['verified', 'revoked'].includes(entry.status) || pairs.has(pair)
      || (entry.status === 'revoked' && !entry.reason)) {
      throw new Error('invalid plugin verification ledger entry')
    }
    pairs.add(pair)
    entries.set(pair, entry)
    if (entry.status === 'verified') verifiedPackages.add(entry.npm)
  }
  if (Object.keys(ledger.current).length !== verifiedPackages.size) throw new Error('invalid current plugin verification selection')
  for (const npm of verifiedPackages) {
    const selected = entries.get(`${npm}@${ledger.current[npm] ?? ''}`)
    if (selected?.status !== 'verified') throw new Error('invalid current plugin verification selection')
  }
  return ledger
}

function fixedLedger(): VerificationLedger {
  const raw = readFileSync(new URL('../compatibility.json', import.meta.url), 'utf8')
  return validateLedger(JSON.parse(raw) as VerificationLedger)
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
export function marketCatalog(community: Registry, ledger: VerificationLedger = fixedLedger()): Registry {
  ledger = validateLedger(ledger)
  const firstParty = fixedCatalog()
  const firstPartyNames = new Set(firstParty.plugins.flatMap(plugin => [plugin.name, plugin.npm].filter((name): name is string => typeof name === 'string')))
  const verification = new Map(ledger.entries.map(entry => [`${entry.npm}@${entry.version}`, entry]))
  const plugins = [
    ...firstParty.plugins,
    ...community.plugins
      .filter(plugin => !firstPartyNames.has(plugin.name) && (plugin.npm === undefined || plugin.npm === null || !firstPartyNames.has(plugin.npm)))
      .map(plugin => {
        const npm = typeof plugin.npm === 'string' ? plugin.npm : undefined
        const selected = npm === undefined ? undefined : verification.get(`${npm}@${ledger.current[npm] ?? ''}`)
        const verified = selected?.repository.toLowerCase() === plugin.url.toLowerCase() && selected.status === 'verified'
        const revoked = verified || npm === undefined
          ? undefined
          : ledger.entries.findLast(entry => entry.npm === npm && entry.status === 'revoked' && entry.repository.toLowerCase() === plugin.url.toLowerCase())
        const record = verified ? selected : revoked
        return {
          ...plugin,
          ...(verified ? { install: plugin.install.replace(plugin.npm!, `${plugin.npm}@${selected.version}`) } : {}),
          catalogSource: 'dsh-community' as const,
          tessivumCompatibility: verified ? 'verified' as const : 'unverified' as const,
          ...(record ? {
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
