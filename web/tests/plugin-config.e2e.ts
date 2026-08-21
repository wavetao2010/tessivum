import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness } from './support'

const SNAPSHOT = join(import.meta.dir, 'snapshots/plugin-config/section.expected.md')

let harness: RustWebHarness | undefined

test('plugin settings stage, validate, save, discard, and reset native overrides', async () => {
  harness = await RustWebHarness.launch({ name: 'plugin-config-web-e2e', locale: 'zh-CN' })
  try {
    const openPlugins = async () => {
      const existing = harness?.page.getByRole('dialog', { name: '设置' })
      if (existing !== undefined && await existing.count() > 0) {
        await harness?.page.keyboard.press('Escape')
        await expect.poll(() => harness?.page.getByRole('dialog', { name: '设置' }).count() ?? 0, { timeout: 5_000 }).toBe(0)
      }
      await harness?.page.getByRole('button', { name: '设置', exact: true }).click()
      const dialog = harness?.page.getByRole('dialog', { name: '设置' })
      if (dialog === undefined) throw new Error('settings dialog did not open')
      await dialog.waitFor({ timeout: 10_000 })
      await dialog.getByRole('button', { name: '插件', exact: true }).click()
      await expect.poll(() => dialog.getByRole('button', { name: '插件', exact: true }).getAttribute('aria-current'), { timeout: 5_000 }).toBe('true')
      await expect.poll(() => dialog.getByRole('tab', { name: '插件配置', exact: true }).getAttribute('aria-selected'), { timeout: 5_000 }).toBe('true')
      return dialog
    }
    const settingsDocument = async (): Promise<string> => readFile(join(harness?.dataDir ?? '', 'settings.yaml'), 'utf8').catch(() => '')

    let dialog = await openPlugins()
    await dialog.getByText('终端', { exact: true }).waitFor({ timeout: 10_000 })
    expect(await dialog.getByText('Agent 循环', { exact: true }).count()).toBe(1)
    expect(await dialog.getByText('网页搜索', { exact: true }).count()).toBe(1)
    expect(await dialog.getByLabel('命令超时（毫秒）').count()).toBe(0)
    expect(await captureStableAria(harness.page, '[role="dialog"]')).toBe((await readFile(SNAPSHOT, 'utf8')).trim())

    dialog = await openPlugins()
    await dialog.getByText('终端', { exact: true }).click()
    const timeout = dialog.getByLabel('命令超时（毫秒）')
    await timeout.waitFor({ timeout: 10_000 })
    expect(await timeout.inputValue()).toBe('60000')
    await timeout.fill('12000')
    await timeout.blur()
    expect(await settingsDocument()).not.toContain('timeoutMs')
    const save = dialog.getByRole('button', { name: '保存', exact: true })
    await expect.poll(() => save.isEnabled(), { timeout: 5_000 }).toBe(true)
    await save.click()
    await expect.poll(async () => (await settingsDocument()).includes('timeoutMs: 12000'), { timeout: 10_000 }).toBe(true)
    await expect.poll(() => dialog.getByText('已覆盖').count(), { timeout: 5_000 }).toBe(1)
    expect(await dialog.getByRole('button', { name: '恢复默认' }).count()).toBe(1)
    await expect.poll(() => save.isDisabled(), { timeout: 5_000 }).toBe(true)

    dialog = await openPlugins()
    await dialog.getByText('终端', { exact: true }).click()
    const discardTimeout = dialog.getByLabel('命令超时（毫秒）')
    await discardTimeout.waitFor({ timeout: 10_000 })
    await discardTimeout.fill('7000')
    await dialog.getByRole('button', { name: '放弃修改' }).click()
    await expect.poll(() => discardTimeout.inputValue(), { timeout: 5_000 }).toBe('12000')
    expect(await settingsDocument()).toContain('timeoutMs: 12000')

    dialog = await openPlugins()
    await dialog.getByText('终端', { exact: true }).click()
    const invalidTimeout = dialog.getByLabel('命令超时（毫秒）')
    await invalidTimeout.waitFor({ timeout: 10_000 })
    await invalidTimeout.fill('soon')
    const invalidSave = dialog.getByRole('button', { name: '保存', exact: true })
    await expect.poll(() => invalidSave.isDisabled(), { timeout: 5_000 }).toBe(true)
    expect(await dialog.getByText('请填数字；留空表示使用默认值。').count()).toBe(1)
    await dialog.getByRole('button', { name: '放弃修改' }).click()

    dialog = await openPlugins()
    await dialog.getByText('终端', { exact: true }).click()
    const resetTimeout = dialog.getByLabel('命令超时（毫秒）')
    await resetTimeout.waitFor({ timeout: 10_000 })
    expect(await resetTimeout.inputValue()).toBe('12000')
    await dialog.getByRole('button', { name: '恢复默认' }).click()
    await expect.poll(() => resetTimeout.inputValue(), { timeout: 5_000 }).toBe('60000')
    expect(await settingsDocument()).toContain('timeoutMs: 12000')
    await dialog.getByRole('button', { name: '保存', exact: true }).click()
    await expect.poll(async () => (await settingsDocument()).includes('timeoutMs'), { timeout: 10_000 }).toBe(false)
    expect(await resetTimeout.inputValue()).toBe('60000')
    expect(await dialog.getByText('已覆盖').count()).toBe(0)
    harness.assertClean()
  } finally {
    await harness.close()
    harness = undefined
  }
}, 120_000)
