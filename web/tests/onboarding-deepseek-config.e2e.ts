import { randomBytes } from 'node:crypto'
import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import {
  acknowledgeReloadConnectionLoss, captureStableAria, RustWebHarness, waitUntil,
} from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/onboarding-deepseek-config')
const WELCOME_EXPECTED = join(SNAPSHOT_DIR, 'welcome.expected.md')
const MISSING_EXPECTED = join(SNAPSHOT_DIR, 'missing.expected.md')
const MODELS_EXPECTED = join(SNAPSHOT_DIR, 'models.expected.md')
const WELCOME_NOTICE_ACK_FIELD = 'welcomeNoticeVersion'
const WELCOME_NOTICE_VERSION = '2026-08-13.1'
const WELCOME_NOTICE_COPY = {
  title: '内测声明',
  body: 'DeepSeek Harness 目前的 0.1 版本仍处在面向 Harness 开发者进行测试的阶段，还有许多地方需要持续改进和打磨，希望听取广大开发者的反馈建议。预计 DeepSeek Harness 的核心插件以及基础 API 都会在接下来的一段时间内快速迭代、持续演化。\n\n我们期待与全球开发者一起，在开源、开放、可复用、可组合的基础设施之上，共同探索智能上限。欢迎全球 Harness 开发者加入 DSH 插件生态。',
  continueLabel: '继续',
} as const

async function expectGolden(harness: RustWebHarness, path: string): Promise<void> {
  expect(await captureStableAria(harness.page, '[role="dialog"]')).toBe((await readFile(path, 'utf8')).trim())
}

let harness: RustWebHarness | undefined

test('first-run DeepSeek credential setup survives native reloads and model edits', async () => {
  harness = await RustWebHarness.launch({
    name: 'onboarding-deepseek-config-web-e2e',
    locale: 'zh-CN',
    showWelcomeNotice: true,
    env: { DEEPSEEK_API_KEY: '' },
  })
  const browserConsole: string[] = []
  harness.page.on('console', message => browserConsole.push(message.text()))
  try {
    const welcome = harness.page.getByRole('dialog', { name: WELCOME_NOTICE_COPY.title })
    await welcome.waitFor({ timeout: 15_000 })
    expect(await harness.page.locator('#root').evaluate(root => (root as HTMLElement).inert)).toBe(true)
    for (const paragraph of WELCOME_NOTICE_COPY.body.split('\n\n')) {
      expect(await welcome.getByText(paragraph, { exact: true }).count()).toBe(1)
    }
    expect(await welcome.getByRole('button').allTextContents()).toEqual([WELCOME_NOTICE_COPY.continueLabel])
    await expectGolden(harness, WELCOME_EXPECTED)

    const firstReloadWarnings = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, firstReloadWarnings)
    await welcome.waitFor({ timeout: 15_000 })

    await welcome.getByRole('button', { name: WELCOME_NOTICE_COPY.continueLabel }).click()
    await welcome.waitFor({ state: 'detached', timeout: 15_000 })
    const credentialStep = harness.page.getByRole('dialog', { name: '添加一个 API Key 开始使用' })
    await credentialStep.waitFor({ timeout: 15_000 })
    const keyInput = credentialStep.getByLabel('API 密钥', { exact: true })
    await keyInput.waitFor({ timeout: 10_000 })
    await expectGolden(harness, MISSING_EXPECTED)

    const secret = `dsh_onboarding_${randomBytes(12).toString('hex')}`
    await keyInput.fill(secret)
    await credentialStep.getByRole('button', { name: '保存并继续' }).click()
    await credentialStep.waitFor({ state: 'detached', timeout: 15_000 })
    expect(await harness.page.locator('#root').evaluate(root => (root as HTMLElement).inert)).toBe(false)

    const stored = await readFile(join(harness.dataDir, 'credentials.yaml'), 'utf8')
    expect(stored.includes(`DEEPSEEK_API_KEY: ${secret}`)).toBe(true)
    expect((await harness.page.content()).includes(secret)).toBe(false)
    expect((await harness.page.locator('body').ariaSnapshot()).includes(secret)).toBe(false)
    expect(browserConsole.some(line => line.includes(secret))).toBe(false)
    const acknowledgedSettings = await readFile(join(harness.dataDir, 'settings.yaml'), 'utf8')
    expect(acknowledgedSettings).toContain(`${WELCOME_NOTICE_ACK_FIELD}: ${WELCOME_NOTICE_VERSION}`)

    await harness.page.getByRole('button', { name: '设置', exact: true }).click()
    const settings = harness.page.getByRole('dialog', { name: '设置' })
    await settings.waitFor({ timeout: 10_000 })
    await settings.getByRole('button', { name: '模型' }).click()
    const deepSeekRow = settings.getByText('DeepSeek', { exact: true }).first()
    await deepSeekRow.waitFor({ timeout: 10_000 })
    await deepSeekRow.locator('xpath=ancestor::li').getByRole('button', { name: '编辑' }).click()
    const configuredInput = settings.getByLabel('API 密钥', { exact: true })
    await configuredInput.waitFor({ timeout: 10_000 })
    await waitUntil(
      () => configuredInput.getAttribute('placeholder'),
      placeholder => placeholder === '已配置——输入新值可替换',
      10_000,
    )

    const secondReloadWarnings = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, secondReloadWarnings)
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 15_000 })
    expect(await harness.page.getByRole('dialog', { name: WELCOME_NOTICE_COPY.title }).count()).toBe(0)
    expect(await harness.page.getByRole('dialog', { name: '添加一个 API Key 开始使用' }).count()).toBe(0)

    const changed = await harness.rpc('settings.mutate', {
      ns: 'ui-onboarding',
      ops: [{ op: 'set', path: [WELCOME_NOTICE_ACK_FIELD], value: 'previous-copy-version' }],
    })
    expect(changed.ok).toBe(true)
    const thirdReloadWarnings = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, thirdReloadWarnings)
    await welcome.waitFor({ timeout: 15_000 })
    await welcome.getByRole('button', { name: WELCOME_NOTICE_COPY.continueLabel }).click()
    await welcome.waitFor({ state: 'detached', timeout: 15_000 })
    expect(await harness.page.getByRole('dialog', { name: '添加一个 API Key 开始使用' }).count()).toBe(0)
    expect((await harness.page.content()).includes(secret)).toBe(false)
    expect((await harness.page.locator('body').ariaSnapshot()).includes(secret)).toBe(false)
    expect(browserConsole.some(line => line.includes(secret))).toBe(false)

    await harness.page.addInitScript(() => {
      const sightings: string[] = []
      ;(window as unknown as { __takeoverSightings: string[] }).__takeoverSightings = sightings
      setInterval(() => {
        if (document.querySelector(
          '[role="dialog"][aria-label="内测声明"], '
          + '[role="dialog"][aria-label="添加一个 API Key 开始使用"]',
        ) !== null) sightings.push('chrome')
        if (document.getElementById('root')?.inert === true) sightings.push('inert')
      }, 8)
    })
    let released = false
    const heldRoutes: Array<() => void> = []
    const releaseDescribe = (): void => {
      released = true
      for (const resolve of heldRoutes.splice(0)) resolve()
    }
    await harness.page.route('**/api/settings.describe', async route => {
      if (!released) await new Promise<void>(resolve => { heldRoutes.push(resolve) })
      await route.continue()
    })
    const warningsBefore = harness.warnings.length
    await harness.page.reload({ waitUntil: 'commit' })
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 15_000 })
    await harness.page.waitForTimeout(600)
    releaseDescribe()
    await harness.page.waitForTimeout(400)
    await harness.page.unroute('**/api/settings.describe')
    acknowledgeReloadConnectionLoss(harness, warningsBefore)
    expect(await harness.page.evaluate(() => (window as unknown as { __takeoverSightings: string[] }).__takeoverSightings)).toEqual([])
    expect(await harness.page.getByRole('dialog', { name: WELCOME_NOTICE_COPY.title }).count()).toBe(0)
    expect(await harness.page.getByRole('dialog', { name: '添加一个 API Key 开始使用' }).count()).toBe(0)

    await harness.page.getByRole('button', { name: '设置', exact: true }).click()
    const modelSettings = harness.page.getByRole('dialog', { name: '设置' })
    await modelSettings.waitFor({ timeout: 10_000 })
    await modelSettings.getByRole('button', { name: '模型' }).click()
    const deepSeek = modelSettings.getByText('DeepSeek', { exact: true }).first()
    await deepSeek.waitFor({ timeout: 10_000 })
    await deepSeek.locator('xpath=ancestor::li').getByRole('button', { name: '编辑' }).click()
    await modelSettings.getByText('自定义设置').click()
    await modelSettings.getByRole('button', { name: /删除模型/ }).first().click()
    await modelSettings.getByRole('button', { name: '添加模型' }).click()
    const customModelId = modelSettings.getByLabel('模型 ID 2')
    await customModelId.fill('private-preview')
    await modelSettings.getByLabel('显示名称 2').fill('Private Preview')
    await modelSettings.getByRole('button', { name: '容量 2' }).click()
    await modelSettings.getByLabel('上下文窗口 2').fill('131072')
    await modelSettings.getByLabel('最大输出 token 数 2').fill('64K')
    await expectGolden(harness, MODELS_EXPECTED)
    await modelSettings.getByRole('button', { name: '保存', exact: true }).click()
    await customModelId.waitFor({ state: 'detached', timeout: 15_000 })

    const document = await readFile(join(harness.dataDir, 'settings.yaml'), 'utf8')
    expect(document).toContain('id: deepseek-v4-pro')
    expect(document).toContain('id: private-preview')
    expect(document).toContain('name: Private Preview')
    expect(document).toContain('contextWindow: 131072')
    expect(document).toContain('maxTokens: 64000')
    expect(document).not.toContain('id: deepseek-v4-flash')

    await harness.page.keyboard.press('Escape')
    const modelTrigger = harness.page.getByRole('button', { name: '选择模型', exact: true })
    await modelTrigger.waitFor({ timeout: 10_000 })
    await modelTrigger.click()
    await harness.page.getByRole('menuitem', { name: /模型/ }).click()
    expect(await harness.page.getByText('deepseek-v4-flash', { exact: true }).count()).toBe(0)
    await harness.page.getByRole('menuitemradio', { name: 'Private Preview' }).waitFor({ timeout: 10_000 })
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
      'missing.expected.md', 'models.expected.md', 'welcome.expected.md',
    ])
    harness.assertClean()
  } finally {
    await harness.close()
    harness = undefined
  }
}, 120_000)
