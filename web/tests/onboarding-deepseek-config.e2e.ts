import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

test('fresh startup is vendor-neutral and model configuration remains opt-in', async () => {
  const harness = await RustWebHarness.launch({
    name: 'neutral-startup-web-e2e',
    locale: 'zh-CN',
    showWelcomeNotice: true,
    preserveCredentialOnboarding: true,
    env: { ANTHROPIC_API_KEY: '', DEEPSEEK_API_KEY: '', OPENAI_API_KEY: '' },
  })
  try {
    const textarea = harness.page.locator('textarea').first()
    await textarea.waitFor({ timeout: 10_000 })
    expect(await harness.page.getByRole('dialog', { name: /内测声明|添加一个 API Key/ }).count()).toBe(0)
    expect(await harness.page.locator('#root').evaluate(root => (root as HTMLElement).inert)).toBe(false)
    expect(await textarea.isEnabled()).toBe(true)
    const modelTrigger = harness.page.getByRole('button', { name: '选择模型', exact: true })
    await modelTrigger.waitFor({ timeout: 10_000 })
    await textarea.fill('未选择模型时保留的草稿')
    await harness.page.getByRole('button', { name: '发送消息', exact: true }).click()
    await harness.page.getByText('发送前请先选择模型', { exact: true }).waitFor({ timeout: 10_000 })
    expect(await textarea.inputValue()).toBe('未选择模型时保留的草稿')
    expect((await harness.sessions()).every(session => session.blank)).toBe(true)

    await harness.page.getByRole('button', { name: '设置', exact: true }).click()
    const settings = harness.page.getByRole('dialog', { name: '设置' })
    await settings.getByRole('button', { name: '模型' }).click()
    const key = settings.getByRole('textbox', { name: 'API 密钥', exact: true })
    await key.fill('sk-neutral-opt-in')
    await settings.getByRole('button', { name: '保存', exact: true }).click()
    await waitUntil(() => key.count(), count => count === 0, 10_000)
    expect(await readFile(join(harness.dataDir, 'credentials.yaml'), 'utf8')).toContain(
      'DEEPSEEK_API_KEY: sk-neutral-opt-in',
    )
    expect(await harness.page.content()).not.toContain('sk-neutral-opt-in')

    await harness.page.keyboard.press('Escape')
    await modelTrigger.click()
    await harness.page.getByRole('menuitem', { name: /模型/ }).click()
    await harness.page.getByRole('menuitemradio', { name: 'DeepSeek-V4-Flash' }).click()
    await harness.page.getByRole('button', { name: /^选择模型，当前 DeepSeek-V4-Flash/ }).waitFor()
    expect(await textarea.inputValue()).toBe('未选择模型时保留的草稿')
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
