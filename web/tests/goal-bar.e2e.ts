import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/goal-bar')
const ACTIVE_EXPECTED = join(SNAPSHOT_DIR, 'active.expected.md')

test('goal bar renders one active goal and clears it without stale chrome', async () => {
  const harness = await RustWebHarness.launch({ name: 'goal-bar', locale: 'en-US' })
  try {
    const input = harness.page.locator('textarea').first()
    await input.waitFor({ timeout: 10_000 })
    await input.fill('/goal guard rapid clear clicks')
    await input.press('Enter')

    const bar = harness.page.locator('[data-goal-bar]')
    await bar.waitFor({ timeout: 10_000 })
    expect(await captureStableAria(harness.page, '[data-goal-bar]')).toBe((await readFile(ACTIVE_EXPECTED, 'utf8')).trim())

    await bar.getByRole('button', { name: 'Clear goal' }).evaluate(button => {
      const control = button as HTMLButtonElement
      control.click()
      control.click()
    })
    await waitUntil(() => harness.page.locator('[data-goal-bar]').count(), count => count === 0, 10_000)
    expect(await harness.page.getByText(/no current goal/iu).count()).toBe(0)
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['active.expected.md'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 60_000)
