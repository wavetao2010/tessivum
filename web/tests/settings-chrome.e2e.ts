import { mkdir, readFile, symlink } from 'node:fs/promises'
import { join } from 'node:path'
import type { Page } from 'playwright-core'
import { expect, test } from 'bun:test'
import { acknowledgeReloadConnectionLoss, captureStableAria, fixture, RustWebHarness, waitUntil } from './support'

interface ThemeState {
  attr: boolean
  background: string
  legacy: string | null
  themeColor: string | null
  themeColorCount: number
  token: string
}

async function openSettings(harness: RustWebHarness, language: 'zh' | 'en' = 'zh') {
  const title = language === 'zh' ? '设置' : 'Settings'
  const trigger = harness.page.getByRole('button', { name: title, exact: true })
  await trigger.click()
  const dialog = harness.page.getByRole('dialog', { name: title })
  await dialog.waitFor({ timeout: 10_000 })
  return { trigger, dialog }
}

function readTheme(target: Page): Promise<ThemeState> {
  return target.evaluate(() => {
    const metas = document.head.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]')
    const computed = getComputedStyle(document.body)
    return {
      attr: document.body.hasAttribute('data-ds-dark-theme'),
      background: computed.backgroundColor,
      legacy: localStorage.getItem('dsh.theme'),
      themeColor: metas[0]?.content ?? null,
      themeColorCount: metas.length,
      token: computed.getPropertyValue('--dsw-alias-bg-base').trim(),
    }
  })
}

function expectThemeColorSynchronized(state: ThemeState): void {
  expect(state.themeColorCount).toBe(1)
  expect(state.background).not.toBe('rgba(0, 0, 0, 0)')
  expect(state.themeColor).toBe(state.background)
}

async function sharedHost(harness: RustWebHarness, name: string): Promise<RustWebHarness> {
  return RustWebHarness.launch({
    name,
    locale: 'zh-CN',
    beforeStart: async candidate => {
      await mkdir(candidate.dataDir, { recursive: true })
      await symlink(join(harness.dataDir, 'settings.yaml'), join(candidate.dataDir, 'settings.yaml'))
    },
  })
}

async function sessionEvents(harness: RustWebHarness, sessionId: string): Promise<Array<{ event: { type: string; data: Record<string, unknown> } }>> {
  const result = await harness.rpc<{ events: Array<{ event: { type: string; data: Record<string, unknown> } }> }>('session.history', { sessionId, maxMessages: 100 })
  if (!result.ok || result.value === undefined) throw new Error(`session.history failed: ${JSON.stringify(result.error)}`)
  return result.value.events
}

test('settings chrome preserves source modal, default, theme, Enter, and locale contracts', async () => {
  const harness = await RustWebHarness.launch({ name: 'settings-chrome', locale: 'zh-CN' })
  try {
    const trigger = harness.page.getByRole('button', { name: '设置', exact: true })
    expect(await trigger.getAttribute('aria-haspopup')).toBe('dialog')
    expect(await trigger.getAttribute('aria-expanded')).toBe('false')
    await trigger.click()
    const dialog = harness.page.getByRole('dialog', { name: '设置' })
    await dialog.waitFor({ timeout: 10_000 })
    expect(await trigger.getAttribute('aria-expanded')).toBe('true')
    expect(await dialog.getByRole('button', { name: '通用设置' }).getAttribute('aria-current')).toBe('true')
    await dialog.getByRole('button', { name: 'Workspace Write' }).waitFor({ timeout: 10_000 })
    await dialog.getByText('语言', { exact: true }).waitFor({ timeout: 5_000 })
    await dialog.getByText('外观', { exact: true }).waitFor({ timeout: 5_000 })
    const openDocument = dialog.getByRole('button', { name: '打开配置文件' })
    await openDocument.waitFor({ timeout: 10_000 })
    let openRequests = 0
    await harness.page.route('**/api/settings.openDocument', async route => {
      const envelope = route.request().postDataJSON() as { rpcId: string; payload: Record<string, never> }
      expect(envelope.payload).toEqual({})
      openRequests += 1
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ type: 'server-response', rpcId: envelope.rpcId, result: { ok: true, value: { opened: true } } }),
      })
    })
    await openDocument.click()
    await waitUntil(() => Promise.resolve(openRequests), value => value === 1, 5_000)
    await waitUntil(() => openDocument.isEnabled(), Boolean, 5_000)
    await harness.page.unroute('**/api/settings.openDocument')
    expect(await captureStableAria(harness.page, '[role="dialog"]')).toBe(
      (await readFile(await fixture('settings-chrome', 'dialog.expected.md'), 'utf8')).trim(),
    )

    await dialog.getByRole('button', { name: '模型' }).click()
    await waitUntil(() => dialog.getByRole('button', { name: '模型' }).getAttribute('aria-current'), value => value === 'true', 5_000)
    expect(await dialog.getByRole('button', { name: '通用设置' }).getAttribute('aria-current')).toBeNull()
    await dialog.getByRole('button', { name: '插件', exact: true }).click()
    await dialog.getByRole('heading', { name: '插件', exact: true }).waitFor({ timeout: 10_000 })
    await dialog.getByRole('tab', { name: '插件列表', exact: true }).click()
    const pluginRow = dialog.locator('[data-plugin-entry$="ui-settings"]')
    await pluginRow.waitFor({ timeout: 10_000 })
    const expectedPluginCount = await harness.page.evaluate(() => {
      const boot = (window as Window & { __DSH_BOOT__?: unknown }).__DSH_BOOT__
      if (Array.isArray(boot)) return boot.length
      if (boot !== null && typeof boot === 'object') {
        const entries = (boot as { entries?: unknown }).entries
        return Array.isArray(entries) ? entries.length : Object.keys(boot).length
      }
      throw new Error('native boot graph is absent')
    })
    expect(await dialog.getByRole('searchbox', { name: '搜索插件' }).count()).toBe(1)
    expect(await dialog.locator('[data-plugin-entry]').count()).toBe(expectedPluginCount)
    expect(await dialog.locator('[data-plugin-count]').getAttribute('data-plugin-count')).toBe(String(expectedPluginCount))
    expect(await dialog.getByRole('button', { name: '插件', exact: true }).getAttribute('aria-current')).toBe('true')
    expect(await dialog.getByRole('tab', { name: '插件列表', exact: true }).getAttribute('aria-selected')).toBe('true')
    expect(await dialog.getByRole('button', { name: '模型' }).getAttribute('aria-current')).toBeNull()
    expect(await captureStableAria(harness.page, '[data-plugin-entry$="ui-settings"]')).toBe(
      (await readFile(await fixture('settings-chrome', 'plugins.expected.md'), 'utf8')).trim(),
    )
    await harness.page.keyboard.press('Escape')
    await waitUntil(() => harness.page.getByRole('dialog', { name: '设置' }).count(), count => count === 0, 5_000)
    expect(await trigger.getAttribute('aria-expanded')).toBe('false')
    await trigger.click()
    await harness.page.getByRole('dialog', { name: '设置' }).getByRole('button', { name: '关闭' }).click()
    await waitUntil(() => harness.page.getByRole('dialog', { name: '设置' }).count(), count => count === 0, 5_000)

    const workspace = await harness.rpc<{ items: Array<{ workspaceId: string }> }>('workspace.list')
    const workspaceId = workspace.value?.items[0]?.workspaceId
    if (workspaceId === undefined) throw new Error('native host has no workspace')
    const existing = await harness.rpc<{ sessionId: string }>('session.create', { sessionId: 'settings-permission-before', workspaceId })
    if (!existing.ok || existing.value === undefined) throw new Error('could not create pre-settings session')
    expect((await sessionEvents(harness, existing.value.sessionId)).find(row => row.event.type === 'permission/preset')?.event.data)
      .toEqual({ preset: 'workspace-write' })

    const general = await openSettings(harness)
    const selector = general.dialog.getByRole('button', { name: 'Workspace Write' })
    await selector.waitFor({ timeout: 10_000 })
    await waitUntil(() => selector.isEnabled(), Boolean, 5_000)
    await selector.click()
    await harness.page.getByRole('menuitem', { name: 'Read Only' }).click()
    await general.dialog.getByRole('button', { name: 'Read Only' }).waitFor({ timeout: 10_000 })
    let document = await readFile(join(harness.dataDir, 'settings.yaml'), 'utf8')
    expect(document).toContain('permission:')
    expect(document).toContain('defaultPreset: read-only')
    expect((await sessionEvents(harness, existing.value.sessionId)).find(row => row.event.type === 'permission/preset')?.event.data)
      .toEqual({ preset: 'workspace-write' })
    const after = await harness.rpc<{ sessionId: string }>('session.create', { sessionId: 'settings-permission-after', workspaceId })
    if (!after.ok || after.value === undefined) throw new Error('could not create post-settings session')
    expect((await sessionEvents(harness, after.value.sessionId))
      .filter(row => ['permission/preset', 'sandbox/mode', 'approval/policy'].includes(row.event.type))
      .map(row => [row.event.type, row.event.data])).toEqual([
      ['permission/preset', { preset: 'read-only' }],
      ['sandbox/mode', { mode: 'read-only' }],
      ['approval/policy', { policy: 'ask' }],
    ])
    await general.dialog.getByRole('button', { name: 'Read Only' }).click()
    await harness.page.getByRole('menuitem', { name: 'Full access' }).click()
    const confirmation = harness.page.getByRole('dialog', { name: '确认启用 Full access？' })
    const enable = confirmation.getByRole('button', { name: '启用 Full access' })
    expect(await enable.isDisabled()).toBe(true)
    await confirmation.getByRole('checkbox').click()
    await enable.click()
    await general.dialog.getByRole('button', { name: 'Full access' }).waitFor({ timeout: 10_000 })
    document = await readFile(join(harness.dataDir, 'settings.yaml'), 'utf8')
    expect(document).toContain('defaultPreset: danger-full-access')
    const confirmed = await harness.rpc<{ sessionId: string }>('session.create', { sessionId: 'settings-permission-confirmed', workspaceId })
    if (!confirmed.ok || confirmed.value === undefined) throw new Error('could not create confirmed session')
    expect((await sessionEvents(harness, confirmed.value.sessionId))
      .filter(row => ['permission/preset', 'sandbox/mode', 'approval/policy'].includes(row.event.type))
      .map(row => [row.event.type, row.event.data])).toEqual([
      ['permission/preset', { preset: 'danger-full-access' }],
      ['sandbox/mode', { mode: 'danger-full-access' }],
      ['approval/policy', { policy: 'never' }],
    ])
    await harness.page.keyboard.press('Escape')

    await harness.page.emulateMedia({ colorScheme: 'light' })
    const initial = await readTheme(harness.page)
    expect(initial.attr).toBe(false)
    expectThemeColorSynchronized(initial)
    const appearance = await openSettings(harness)
    const darkCube = appearance.dialog.getByRole('button', { name: '深色' })
    expect(await darkCube.getAttribute('aria-pressed')).toBe('false')
    await darkCube.click()
    await waitUntil(() => darkCube.getAttribute('aria-pressed'), value => value === 'true', 5_000)
    const dark = await readTheme(harness.page)
    expect(dark.attr).toBe(true)
    expect(dark.legacy).toBeNull()
    expect(dark.token).not.toBe(initial.token)
    expectThemeColorSynchronized(dark)
    await waitUntil(() => readFile(join(harness.dataDir, 'settings.yaml'), 'utf8'), value => /ui-theme:\n\s+preference: dark/.test(value), 5_000)
    await harness.page.keyboard.press('Escape')
    const themeWarningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    acknowledgeReloadConnectionLoss(harness, themeWarningStart)
    await harness.page.emulateMedia({ colorScheme: 'light' })
    await waitUntil(async () => (await readTheme(harness.page)).attr, Boolean, 5_000)
    const reloaded = await readTheme(harness.page)
    expect(reloaded.legacy).toBeNull()
    expectThemeColorSynchronized(reloaded)
    const second = await sharedHost(harness, 'settings-chrome-shared-theme')
    try {
      expect(second.baseUrl).not.toBe(harness.baseUrl)
      await second.page.emulateMedia({ colorScheme: 'light' })
      await waitUntil(async () => (await readTheme(second.page)).attr, Boolean, 5_000)
      const secondState = await readTheme(second.page)
      expect(secondState.legacy).toBeNull()
      expectThemeColorSynchronized(secondState)
      second.assertClean()
    } finally { await second.close() }
    const restored = await openSettings(harness)
    const systemCube = restored.dialog.getByRole('button', { name: '跟随系统' })
    await systemCube.click()
    await waitUntil(() => systemCube.getAttribute('aria-pressed'), value => value === 'true', 5_000)
    await waitUntil(async () => !(await readTheme(harness.page)).attr, Boolean, 5_000)
    expectThemeColorSynchronized(await readTheme(harness.page))
    await harness.page.emulateMedia({ colorScheme: 'dark' })
    await waitUntil(async () => (await readTheme(harness.page)).attr, Boolean, 5_000)
    expectThemeColorSynchronized(await readTheme(harness.page))
    await restored.dialog.getByRole('button', { name: '浅色' }).click()
    await waitUntil(async () => !(await readTheme(harness.page)).attr, Boolean, 5_000)
    expectThemeColorSynchronized(await readTheme(harness.page))
    await harness.page.keyboard.press('Escape')

    const enter = await openSettings(harness)
    await enter.dialog.getByRole('button', { name: '排队发送' }).click()
    await harness.page.getByRole('menuitem', { name: '插话发送' }).click()
    await enter.dialog.getByRole('button', { name: '插话发送' }).waitFor({ timeout: 10_000 })
    expect(await harness.page.evaluate(() => localStorage.getItem('dsh.conversation.busyEnter'))).toBeNull()
    await waitUntil(() => readFile(join(harness.dataDir, 'settings.yaml'), 'utf8'), value => /ui-conversation:\n\s+busyEnter: steer/.test(value), 5_000)
    await harness.page.keyboard.press('Escape')
    const enterWarningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    acknowledgeReloadConnectionLoss(harness, enterWarningStart)
    const reloadedEnter = await openSettings(harness)
    await reloadedEnter.dialog.getByRole('button', { name: '插话发送' }).waitFor({ timeout: 10_000 })
    const secondEnter = await sharedHost(harness, 'settings-chrome-shared-enter')
    try {
      expect(secondEnter.baseUrl).not.toBe(harness.baseUrl)
      const secondDialog = await openSettings(secondEnter)
      await secondDialog.dialog.getByRole('button', { name: '插话发送' }).waitFor({ timeout: 10_000 })
      expect(await secondEnter.page.evaluate(() => localStorage.getItem('dsh.conversation.busyEnter'))).toBeNull()
      secondEnter.assertClean()
    } finally { await secondEnter.close() }
    await reloadedEnter.dialog.getByRole('button', { name: '插话发送' }).click()
    await harness.page.getByRole('menuitem', { name: '排队发送' }).click()
    await reloadedEnter.dialog.getByRole('button', { name: '排队发送' }).waitFor({ timeout: 10_000 })
    expect(await harness.page.evaluate(() => localStorage.getItem('dsh.conversation.busyEnter'))).toBeNull()
    await waitUntil(() => readFile(join(harness.dataDir, 'settings.yaml'), 'utf8'), value => /ui-conversation:\n\s+busyEnter: queue/.test(value), 5_000)
    await harness.page.keyboard.press('Escape')

    const language = await openSettings(harness)
    const languageSelector = language.dialog.getByRole('button', { name: '中文' })
    expect(await languageSelector.getAttribute('aria-haspopup')).toBe('menu')
    await languageSelector.click()
    await harness.page.getByRole('menuitem', { name: 'English' }).click()
    const enDialog = harness.page.getByRole('dialog', { name: 'Settings' })
    await enDialog.waitFor({ timeout: 10_000 })
    expect(await enDialog.getByRole('button', { name: 'General' }).getAttribute('aria-current')).toBe('true')
    await enDialog.getByText('Appearance', { exact: true }).waitFor({ timeout: 5_000 })
    expect(await harness.page.evaluate(() => localStorage.getItem('dsh.locale'))).toBeNull()
    await waitUntil(() => readFile(join(harness.dataDir, 'settings.yaml'), 'utf8'), value => /locale:\n\s+preference: en/.test(value), 5_000)
    const languageWarningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    acknowledgeReloadConnectionLoss(harness, languageWarningStart)
    const enTrigger = harness.page.getByRole('button', { name: 'Settings' })
    await enTrigger.waitFor({ timeout: 10_000 })
    const secondLanguage = await sharedHost(harness, 'settings-chrome-shared-language')
    try {
      expect(secondLanguage.baseUrl).not.toBe(harness.baseUrl)
      const secondDialog = await openSettings(secondLanguage, 'en')
      await secondDialog.dialog.getByRole('button', { name: 'English' }).waitFor({ timeout: 10_000 })
      expect(await secondLanguage.page.evaluate(() => localStorage.getItem('dsh.locale'))).toBeNull()
      secondLanguage.assertClean()
    } finally { await secondLanguage.close() }
    await enTrigger.click()
    await harness.page.getByRole('dialog', { name: 'Settings' }).getByRole('button', { name: 'English' }).click()
    await harness.page.getByRole('menuitem', { name: '中文' }).click()
    await harness.page.getByRole('dialog', { name: '设置' }).waitFor({ timeout: 10_000 })
    expect(await harness.page.evaluate(() => localStorage.getItem('dsh.locale'))).toBeNull()
    await waitUntil(() => readFile(join(harness.dataDir, 'settings.yaml'), 'utf8'), value => /locale:\n\s+preference: zh/.test(value), 5_000)
    await harness.page.keyboard.press('Escape')
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 240_000)

test('an English browser opens settings in English without a stored preference', async () => {
  const harness = await RustWebHarness.launch({ name: 'settings-chrome-browser-language', locale: 'en-US' })
  try {
    expect(await harness.page.evaluate(() => localStorage.getItem('dsh.locale'))).toBeNull()
    const { dialog } = await openSettings(harness, 'en')
    await dialog.getByRole('button', { name: 'English' }).waitFor({ timeout: 10_000 })
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 90_000)
