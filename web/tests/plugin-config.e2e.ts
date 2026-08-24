import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness, waitUntil } from './support'

const SNAPSHOT = join(import.meta.dir, 'snapshots/plugin-config/section.expected.md')

let harness: RustWebHarness | undefined

test('plugin settings stage, validate, save, discard, and reset native overrides', async () => {
  harness = await RustWebHarness.launch({ name: 'plugin-config-web-e2e', locale: 'zh-CN' })
  try {
    const openPlugins = async () => {
      const existing = harness?.page.getByRole('dialog', { name: '设置' })
      if (existing !== undefined && await existing.count() > 0) {
        await harness?.page.keyboard.press('Escape')
        await waitUntil(() => harness?.page.getByRole('dialog', { name: '设置' }).count() ?? 0, count => count === 0, 5_000)
      }
      await harness?.page.getByRole('button', { name: '设置', exact: true }).click()
      const dialog = harness?.page.getByRole('dialog', { name: '设置' })
      if (dialog === undefined) throw new Error('settings dialog did not open')
      await dialog.waitFor({ timeout: 10_000 })
      await dialog.getByRole('button', { name: '插件', exact: true }).click()
      await waitUntil(() => dialog.getByRole('button', { name: '插件', exact: true }).getAttribute('aria-current'), value => value === 'true', 5_000)
      await waitUntil(() => dialog.getByRole('tab', { name: '插件配置', exact: true }).getAttribute('aria-selected'), value => value === 'true', 5_000)
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
    await waitUntil(() => save.isEnabled(), enabled => enabled, 5_000)
    await save.click()
    await waitUntil(async () => (await settingsDocument()).includes('timeoutMs: 12000'), saved => saved, 10_000)
    await waitUntil(() => dialog.getByText('已覆盖').count(), count => count === 1, 5_000)
    expect(await dialog.getByRole('button', { name: '恢复默认' }).count()).toBe(1)
    await waitUntil(() => save.isDisabled(), disabled => disabled, 5_000)

    dialog = await openPlugins()
    await dialog.getByText('终端', { exact: true }).click()
    const discardTimeout = dialog.getByLabel('命令超时（毫秒）')
    await discardTimeout.waitFor({ timeout: 10_000 })
    await discardTimeout.fill('7000')
    await dialog.getByRole('button', { name: '放弃修改' }).click()
    await waitUntil(() => discardTimeout.inputValue(), value => value === '12000', 5_000)
    expect(await settingsDocument()).toContain('timeoutMs: 12000')

    dialog = await openPlugins()
    await dialog.getByText('终端', { exact: true }).click()
    const invalidTimeout = dialog.getByLabel('命令超时（毫秒）')
    await invalidTimeout.waitFor({ timeout: 10_000 })
    await invalidTimeout.fill('soon')
    const invalidSave = dialog.getByRole('button', { name: '保存', exact: true })
    await waitUntil(() => invalidSave.isDisabled(), disabled => disabled, 5_000)
    expect(await dialog.getByText('请填数字；留空表示使用默认值。').count()).toBe(1)
    await dialog.getByRole('button', { name: '放弃修改' }).click()

    dialog = await openPlugins()
    await dialog.getByText('终端', { exact: true }).click()
    const resetTimeout = dialog.getByLabel('命令超时（毫秒）')
    await resetTimeout.waitFor({ timeout: 10_000 })
    expect(await resetTimeout.inputValue()).toBe('12000')
    await dialog.getByRole('button', { name: '恢复默认' }).click()
    await waitUntil(() => resetTimeout.inputValue(), value => value === '60000', 5_000)
    expect(await settingsDocument()).toContain('timeoutMs: 12000')
    await dialog.getByRole('button', { name: '保存', exact: true }).click()
    await waitUntil(async () => (await settingsDocument()).includes('timeoutMs'), present => !present, 10_000)
    expect(await resetTimeout.inputValue()).toBe('60000')
    expect(await dialog.getByText('已覆盖').count()).toBe(0)
    expect(await readdir(join(import.meta.dir, 'snapshots/plugin-config'))).toEqual(['section.expected.md'])
    harness.assertClean()
  } finally {
    await harness.close()
    harness = undefined
  }
}, 120_000)
