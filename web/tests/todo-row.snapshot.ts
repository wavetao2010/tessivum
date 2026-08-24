import { expect, test } from 'bun:test'
import type { Locator } from 'playwright-core'
import { openSnapshotSession, seedSnapshotFixture } from './snapshot-fixture'
import { RustWebHarness } from './support'

const GOLDEN = `${import.meta.dir}/snapshots/todo-row/parallel-plan.expected.txt`

async function todoShape(row: Locator, panel: Locator): Promise<string> {
  const rowShape = await row.evaluate(root => {
    const has = (element: Element, name: string): boolean => [...element.classList].some(cls => cls === name || cls.endsWith(`_${name}`) || cls.startsWith(`_${name}_`) || cls.includes(`_${name}_`))
    const pick = (from: Element, name: string): Element[] => [...from.querySelectorAll('*')].filter(element => has(element, name))
    const first = (from: Element, name: string): string => pick(from, name)[0]?.textContent?.trim() ?? '<absent>'
    return [
      `row=${root.getAttribute('data-tool')}`,
      `title=${first(root, 'title')}`,
      `summary=${first(root, 'summary')}`,
      `suffix=${first(root, 'summarySuffix')}`,
    ].join('\n')
  })
  const panelShape = await panel.evaluate(root => {
    const has = (element: Element, name: string): boolean => [...element.classList].some(cls => cls === name || cls.endsWith(`_${name}`) || cls.startsWith(`_${name}_`) || cls.includes(`_${name}_`))
    const first = (name: string): string => [...root.querySelectorAll('*')]
      .find(element => has(element, name))?.textContent?.trim() ?? '<absent>'
    const items = [...root.querySelectorAll('[data-status]')]
      .map(item => `item=${item.getAttribute('data-status')} ${item.textContent?.trim() ?? ''}`)
    return [`panel=${first('progress')}`, ...items].join('\n')
  })
  return `${rowShape}\n${panelShape}`
}
test('renders the parallel plan as a row summary, separate active count, and dock plan strip', async () => {
  const harness = await RustWebHarness.launch({ name: 'todo-row-snapshot' })
  try {
    await seedSnapshotFixture(harness)
    await openSnapshotSession(harness)
    const row = harness.page.locator('[data-tool="todo_write"]').first()
    await row.waitFor({ timeout: 10_000 })
    const panel = harness.page.getByTestId('todo-panel')
    await panel.waitFor({ timeout: 10_000 })
    const toggle = panel.locator('button[aria-expanded]').first()
    if (await toggle.getAttribute('aria-expanded') === 'false') await toggle.click()
    const shape = await todoShape(row, panel)
    const expected = (await Bun.file(GOLDEN).text()).trim()
    expect(shape).toBe(expected)
    harness.assertClean()
  } finally {
    await harness.close()
  }
})
