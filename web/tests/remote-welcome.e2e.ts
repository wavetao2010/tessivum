import { expect, test } from 'bun:test'
import { acknowledgeReloadConnectionLoss, RustWebHarness, waitUntil } from './support'

test('remote welcome advances process-locally and returns after reload', async () => {
  const harness = await RustWebHarness.launch({
    name: 'remote-welcome',
    locale: 'zh-CN',
    remoteAuthority: 'remote.localhost',
    showWelcomeNotice: true,
  })
  try {
    const welcome = harness.page.getByRole('dialog', { name: '内测声明' })
    await welcome.waitFor({ timeout: 15_000 })
    expect(await harness.page.locator('#root').evaluate(root => (root as HTMLElement).inert)).toBe(true)

    await welcome.getByRole('button', { name: '继续' }).click()
    await welcome.waitFor({ state: 'detached', timeout: 15_000 })
    await waitUntil(
      () => harness.page.locator('#root').evaluate(root => (root as HTMLElement).inert),
      inert => !inert,
    )

    const warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await welcome.waitFor({ timeout: 15_000 })
    expect(await harness.page.locator('#root').evaluate(root => (root as HTMLElement).inert)).toBe(true)
    const expectedRejections = harness.httpErrors.splice(0)
    expect(expectedRejections.length).toBeGreaterThan(0)
    expect(expectedRejections.every(error => / 403 |^403 /.test(error)
      && /\/api\/(?:settings\.describe|credentials\.describe)/.test(error))).toBe(true)
    const expectedWarnings = harness.warnings.splice(0)
    expect(expectedWarnings.length).toBe(expectedRejections.length)
    expect(expectedWarnings.every(warning => /status of 403 \(Forbidden\)/.test(warning))).toBe(true)
    harness.assertClean()
  } finally {
    await harness.close()
  }
})
