import { expect, test } from 'bun:test'
import type { Locator } from 'playwright-core'
import { openSnapshotSession, seedSnapshotFixture } from './snapshot-fixture'
import { RustWebHarness } from './support'

const GOLDEN = `${import.meta.dir}/snapshots/search-card/grep-card.expected.txt`

async function cardShape(row: Locator): Promise<string> {
  return row.evaluate(root => {
    const has = (element: Element, name: string): boolean => [...element.classList].some(cls => cls === name || cls.endsWith(`_${name}`) || cls.startsWith(`_${name}_`) || cls.includes(`_${name}_`))
    const card = root.querySelector('[data-search]')
    if (card === null) return '<no search card>'
    const pick = (name: string): Element[] => [...card.querySelectorAll('*')].filter(element => has(element, name))
    const lines = [`kind=${card.getAttribute('data-search')}`]
    const summary = pick('summary')[0]?.textContent?.trim()
    if (summary !== undefined && summary !== '') lines.push(`summary=${summary}`)
    for (const header of pick('fileHeader')) lines.push(`file=${header.textContent?.trim() ?? ''}`)
    for (const line of pick('line')) lines.push(`line=${line.textContent?.trim() ?? ''}`)
    const expand = pick('expand')[0]?.textContent?.trim()
    if (expand !== undefined && expand !== '') lines.push(`expand=${expand}`)
    const recovery = [...root.querySelectorAll('*')].find(element => has(element, 'searchRecovery'))?.textContent?.trim()
    if (recovery !== undefined && recovery !== '') lines.push(`recovery=${recovery}`)
    return lines.join('\n')
  })
}

test('renders the grep card, truncation summary, and capped head/tail slice from native history', async () => {
  const harness = await RustWebHarness.launch({ name: 'search-card-snapshot' })
  try {
    await seedSnapshotFixture(harness)
    await openSnapshotSession(harness)
    const row = harness.page.locator('[data-tool="grep"]').first()
    await row.waitFor({ timeout: 10_000 })
    const expandable = row.locator('[data-expandable]').first()
    if (await expandable.count() > 0) await expandable.click()
    await row.locator('[data-search]').first().waitFor({ timeout: 10_000 })
    expect(await cardShape(row)).toBe((await Bun.file(GOLDEN).text()).trim())
    harness.assertClean()
  } finally {
    await harness.close()
  }
})
