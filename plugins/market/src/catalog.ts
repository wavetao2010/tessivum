import { readFileSync } from 'node:fs'
import type { CatalogSource, Registry, RegistryPlugin, TessivumCompatibility } from './registry.ts'

interface FixedCatalog extends Registry {
  plugins: Array<RegistryPlugin & {
    catalogSource: CatalogSource
    tessivumCompatibility: TessivumCompatibility
  }>
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
export function marketCatalog(community: Registry): Registry {
  const firstParty = fixedCatalog()
  const plugins = [
    ...firstParty.plugins,
    ...community.plugins.map(plugin => ({
      ...plugin,
      catalogSource: 'dsh-community' as const,
      tessivumCompatibility: plugin.tessivumCompatibility === 'verified' ? 'verified' as const : 'unverified' as const,
    })),
  ]
  return {
    ...community,
    updated: firstParty.updated,
    count: plugins.length,
    categories: { ...community.categories, ...firstParty.categories },
    plugins,
  }
}
