import { expect, test } from 'bun:test'
import { openSnapshotSession, seedSnapshotFixture } from './snapshot-fixture'
import { RustWebHarness } from './support'

test('boots the native plugin graph and renders a fixture session end to end', async () => {
  const harness = await RustWebHarness.launch({ name: 'built-boot-snapshot' })
  try {
    await seedSnapshotFixture(harness)
    await openSnapshotSession(harness)
    const row = harness.page.getByRole('treeitem', { name: /Fixture 历史会话/ })
    expect(await row.locator('[data-state="warning"]').count()).toBe(0)
    await harness.page.getByRole('paragraph').filter({ hasText: 'Fixture history ready.' }).waitFor({ timeout: 10_000 })
    const bash = harness.page.locator('[data-sample="bash"]').first()
    await bash.waitFor({ timeout: 10_000 })
    const search = harness.page.locator('[data-tool="grep"]').first()
    await search.waitFor({ timeout: 10_000 })
    const expandable = search.locator('[data-expandable]').first()
    if (await expandable.count() !== 0) await expandable.click()
    await search.locator('[data-search]').waitFor({ timeout: 10_000 })
    await harness.page.locator('[data-tool="todo_write"]').waitFor({ timeout: 10_000 })
    await harness.page.getByTestId('todo-panel').waitFor({ timeout: 10_000 })
    const styleOwners = await harness.page.locator('style[data-plugin]').evaluateAll(styles => styles.map(style => style.getAttribute('data-plugin')))
    expect(styleOwners).toContain('@deepseek-ai/dsh-client-ui-layout')
    expect(styleOwners).toContain('@deepseek-ai/dsh-client-ui-sidebar')
    expect(styleOwners).toContain('@deepseek-ai/dsh-client-ui-conversation')
    expect(styleOwners).toContain('@deepseek-ai/dsh-client-ui-tool')
    harness.assertClean()
  } finally {
    await harness.close()
  }
})
