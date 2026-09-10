import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'


test('goal bar renders one active goal and clears it without stale chrome', async () => {
  const harness = await RustWebHarness.launch({
    name: 'goal-bar', locale: 'en-US',
    env: { OPENAI_MODEL: 'fixture', OPENAI_BASE_URL: 'http://127.0.0.1:1', TESSIVUM_LLM_AUTH: 'none' },
  })
  try {
    const input = harness.page.locator('textarea').first()
    await input.waitFor({ timeout: 10_000 })
    await input.fill('/goal guard rapid clear clicks')
    await input.press('Enter')

    const bar = harness.page.locator('[data-goal-bar]')
    await bar.waitFor({ timeout: 10_000 })

    await bar.getByRole('button', { name: 'Clear goal' }).evaluate(button => {
      const control = button as HTMLButtonElement
      control.click()
      control.click()
    })
    await waitUntil(() => harness.page.locator('[data-goal-bar]').count(), count => count === 0, 10_000)
    expect(await harness.page.getByText(/no current goal/iu).count()).toBe(0)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 60_000)
