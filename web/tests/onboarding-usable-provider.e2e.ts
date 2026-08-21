import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { acknowledgeReloadConnectionLoss, captureStableAria, RustWebHarness } from './support'

const SNAPSHOT = join(import.meta.dir, 'snapshots/onboarding-usable-provider/dismissed.expected.md')
const CREDENTIAL_STEP = '添加一个 API Key 开始使用'

let harness: RustWebHarness | undefined

test('another usable provider ends first-run onboarding', async () => {
  harness = await RustWebHarness.launch({
    name: 'onboarding-usable-provider-web-e2e',
    locale: 'zh-CN',
    env: { DEEPSEEK_API_KEY: '' },
  })
  try {
    const credentialStep = harness.page.getByRole('dialog', { name: CREDENTIAL_STEP })
    await credentialStep.waitFor({ timeout: 15_000 })
    await credentialStep.getByRole('button', { name: '稍后配置' }).click()
    await credentialStep.waitFor({ state: 'detached', timeout: 15_000 })

    await harness.page.getByRole('button', { name: '设置', exact: true }).click()
    const settings = harness.page.getByRole('dialog', { name: '设置' })
    await settings.waitFor({ timeout: 10_000 })
    await settings.getByRole('button', { name: '模型' }).click()
    const setupKey = settings.getByRole('textbox', { name: 'API 密钥', exact: true })
    await setupKey.waitFor({ timeout: 10_000 })
    const add = settings.getByRole('button', { name: '添加提供方' })
    await expect.poll(() => add.isEnabled(), { timeout: 10_000 }).toBe(true)
    await add.click()
    const pick = settings.getByLabel('提供方')
    await pick.waitFor({ timeout: 10_000 })
    await pick.selectOption('minimax-cn')
    await expect.poll(
      () => settings.getByRole('textbox', { name: 'API 密钥', exact: true }).count(),
      { timeout: 10_000 },
    ).toBe(2)
    await settings.getByRole('button', { name: '取消', exact: true }).first().click()
    expect(await settings.getByLabel('提供方').count()).toBe(1)
    await expect.poll(
      () => settings.getByRole('textbox', { name: 'API 密钥', exact: true }).count(),
      { timeout: 10_000 },
    ).toBe(1)
    await settings.getByRole('button', { name: '编辑 DeepSeek (deepseek-official)' }).waitFor({ timeout: 10_000 })
    expect(await captureStableAria(harness.page, '[role="dialog"]')).toBe((await readFile(SNAPSHOT, 'utf8')).trim())

    await settings.getByRole('textbox', { name: 'API 密钥', exact: true }).fill('sk-e2e-minimax')
    await settings.getByRole('button', { name: '保存', exact: true }).click()
    await settings.getByText('已保存 minimax-cn。', { exact: true }).waitFor({ timeout: 15_000 })
    const document = await readFile(join(harness.dataDir, 'settings.yaml'), 'utf8')
    expect(document).toContain('apiKeyEnv: MINIMAX_CN_API_KEY')
    const credentials = await readFile(join(harness.dataDir, 'credentials.yaml'), 'utf8')
    expect(credentials).toContain('MINIMAX_CN_API_KEY: sk-e2e-minimax')
    expect(credentials).not.toContain('DEEPSEEK_API_KEY')

    const warningsBefore = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, warningsBefore)
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 15_000 })
    await expect.poll(
      () => harness?.page.getByRole('dialog', { name: CREDENTIAL_STEP }).count() ?? 0,
      { timeout: 10_000 },
    ).toBe(0)
    expect(await harness.page.locator('#root').evaluate(root => (root as HTMLElement).inert)).toBe(false)

    await harness.page.getByRole('button', { name: '设置', exact: true }).click()
    await settings.waitFor({ timeout: 10_000 })
    await settings.getByRole('button', { name: '模型' }).click()
    await settings.getByRole('button', { name: '编辑 DeepSeek (deepseek-official)' }).waitFor({ timeout: 10_000 })
    expect(await settings.getByRole('textbox', { name: 'API 密钥', exact: true }).count()).toBe(0)
    expect((await harness.page.content()).includes('sk-e2e-minimax')).toBe(false)
    harness.assertClean()
  } finally {
    await harness.close()
    harness = undefined
  }
}, 120_000)
