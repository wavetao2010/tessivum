import { expect, test } from 'bun:test'
import { openSnapshotSession, seedSnapshotFixture } from './snapshot-fixture'
import { RustWebHarness } from './support'

const EXPECTED = `${import.meta.dir}/snapshots/max-tokens-notice/history-turn.expected.txt`

test('renders the localized truncation notice after the cut-off answer instead of ending silently', async () => {
  const harness = await RustWebHarness.launch({ name: 'max-tokens-notice-snapshot' })
  try {
    await seedSnapshotFixture(harness)
    await openSnapshotSession(harness)
    await harness.page.getByText(/条目 3：这一条写到一半被/).waitFor({ timeout: 10_000 })
    const row = harness.page.locator('[role="status"]').filter({ hasText: 'Output token limit reached' }).first()
    await row.waitFor({ timeout: 10_000 })
    const title = row.locator('[class*="maxTokensTitle"]').first()
    const hint = row.locator('[class*="turnErrorMessage"]').first()
    const shape = `dot=${await row.locator('[data-state]').getAttribute('data-state')}\ntitle=${await title.textContent()}\nhint=${await hint.textContent()}`
    expect(shape).toBe((await Bun.file(EXPECTED).text()).trim())
    harness.assertClean()
  } finally {
    await harness.close()
  }
})
