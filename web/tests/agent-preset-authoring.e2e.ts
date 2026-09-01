import { afterAll, beforeAll, expect, test } from 'bun:test'
import { existsSync } from 'node:fs'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import type { Locator } from 'playwright-core'
import { RustWebHarness } from './support.ts'

const SNAPSHOTS = join(import.meta.dir, 'snapshots/agent-mode-authoring')

let harness: RustWebHarness
let modeRoot: string
const BUILT_IN_MODES = [
  ['标准模式', '提供完整的 Tessivum 原生工具集。'],
  ['PTC 模式', '由 Bun 驱动的程序化工具调用方式。'],
  ['极简模式', '专注于持久 Shell 编辑的精简模式。'],
  ['组装模式', '用于构建 Native、WASM 或 Legacy 条目组合的原生模式。'],
] as const

function normalize(snapshot: string): string {
  return snapshot
    .replaceAll(modeRoot, '{{modeRoot}}')
    .split('\n')
    .map(line => line.includes('- code: {{modeRoot}}/')
      ? `${line.replace('- code: ', "- code: '")}'`
      : line.includes('alert: "无法加载 Agent 模式。')
        ? `${line.slice(0, line.indexOf('alert:'))}alert: "{{modeError}}"`
        : line)
    .join('\n')
    .trim()
}

async function expectGolden(locator: Locator, name: string): Promise<void> {
  expect(normalize(await locator.ariaSnapshot())).toBe((await Bun.file(join(SNAPSHOTS, name)).text()).trim())
}
async function expectLocalizedModeMenu(anchor: Locator): Promise<void> {
  const menu = harness.page.getByRole('menu')
  await menu.waitFor()
  for (const [name, description] of BUILT_IN_MODES) {
    expect(await menu.getByRole('menuitem', { name: `${name} ${description}`, exact: true }).count()).toBe(1)
  }
  await anchor.click()
  await menu.waitFor({ state: 'detached' })
}

beforeAll(async () => {
  harness = await RustWebHarness.launch({ name: 'agent-mode-authoring', locale: 'zh-CN' })
  modeRoot = join(harness.dataDir, 'modes')
}, 120_000)

afterAll(async () => { await harness?.close() })

test('Agent Mode UI localizes built-ins and manages Host-owned mode.toml', async () => {
  const seatMode = harness.page.getByRole('button', { name: '标准模式', exact: true })
  await seatMode.click()
  await expectLocalizedModeMenu(seatMode)

  await harness.page.getByRole('button', { name: '设置', exact: true }).click()
  const settings = harness.page.getByRole('dialog', { name: '设置' })
  const defaultMode = settings.getByRole('button', { name: '标准模式', exact: true })
  await defaultMode.click()
  await expectLocalizedModeMenu(defaultMode)
  await settings.getByRole('button', { name: 'Agent 模式' }).click()
  await settings.getByText('标准模式').first().waitFor({ timeout: 10_000 })
  await expectGolden(settings, 'section.expected.yml')

  await settings.getByRole('button', { name: '查看: 标准模式' }).click()
  const viewer = harness.page.getByRole('dialog', { name: '查看 mode.toml · 标准模式' })
  await viewer.waitFor()
  expect(await viewer.locator('pre').textContent()).toContain('id = "standard"')
  expect(await viewer.getByRole('textbox').count()).toBe(0)
  await viewer.getByRole('button', { name: '关闭' }).last().click()

  for (const name of ['标准模式', 'PTC 模式', '极简模式', '组装模式']) {
    expect(await settings.getByRole('button', { name: `编辑: ${name}` }).count()).toBe(0)
    expect(await settings.getByRole('button', { name: `删除: ${name}` }).count()).toBe(0)
  }

  await settings.getByRole('button', { name: '复制: 极简模式' }).click()
  const copy = harness.page.getByRole('dialog', { name: '复制 Agent 模式 · 复制自 极简模式' })
  await expectGolden(copy, 'copy-dialog.expected.yml')
  await copy.getByPlaceholder('my-mode').fill('my-mode')
  await copy.getByPlaceholder('选择器中显示的名字，缺省用标识符').fill('我的模式')
  await copy.getByRole('button', { name: '创建' }).click()
  await copy.waitFor({ state: 'detached' })
  const modeFile = join(modeRoot, 'my-mode/mode.toml')
  await settings.getByText(modeFile).waitFor()
  const customSection = settings.getByRole('heading', { name: '自定义' }).locator('..')
  await expectGolden(customSection, 'created.expected.yml')

  const authored = await readFile(modeFile, 'utf8')
  expect(authored).toContain('id = "my-mode"')
  expect(authored).toContain('name = "我的模式"')

  await settings.getByRole('button', { name: '编辑: 我的模式' }).click()
  await settings.getByText(modeFile).waitFor()

  await settings.getByRole('button', { name: '复制: 我的模式' }).click()
  const secondCopy = harness.page.getByRole('dialog', { name: '复制 Agent 模式 · 复制自 我的模式' })
  await secondCopy.getByPlaceholder('my-mode').fill('my-copy')
  await secondCopy.getByRole('button', { name: '创建' }).click()
  await secondCopy.waitFor({ state: 'detached' })
  const copied = await readFile(join(modeRoot, 'my-copy/mode.toml'), 'utf8')
  expect(copied).toContain('id = "my-copy"')
  expect(copied).toContain('name = "我的模式"')

  for (const id of ['my-mode', 'my-copy']) {
    const idCode = harness.page.locator('code').filter({ hasText: new RegExp(`^${id}$`) })
    const row = settings.getByRole('listitem').filter({ has: idCode })
    await row.getByRole('button', { name: /^删除:/ }).click()
    const confirm = harness.page.getByRole('dialog', { name: '删除该 Agent 模式？' })
    await confirm.getByRole('button', { name: '删除', exact: true }).click()
    await confirm.waitFor({ state: 'detached' })
    expect(existsSync(join(modeRoot, id))).toBe(false)
  }

  await mkdir(join(modeRoot, 'broken'), { recursive: true })
  await writeFile(join(modeRoot, 'broken/mode.toml'), [
    'schema = 1',
    'id = "broken"',
    'name = "Broken"',
    'description = "Invalid fixture"',
    'unknown = true',
    '',
  ].join('\n'))
  await settings.getByRole('button', { name: '通用设置' }).click()
  await settings.getByRole('button', { name: 'Agent 模式' }).click()
  await settings.getByRole('alert').waitFor()
  await expectGolden(settings, 'error.expected.yml')

  harness.assertClean()
}, 120_000)
