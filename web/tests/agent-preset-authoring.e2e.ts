import { afterAll, beforeAll, expect, test } from 'bun:test'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { chromium, type Browser, type Locator, type Page } from 'playwright-core'

const HERE = dirname(fileURLToPath(import.meta.url))
const CRATE_ROOT = join(HERE, '../..')
const SHIPPED_PRESETS = join(process.env.TESSIVUM_DEEPSEEK_SOURCE ?? join(CRATE_ROOT, '../upstream/deepseek-harness'), 'apps/cli/config/agent-presets')
const SNAPSHOTS = join(HERE, 'snapshots/agent-preset-authoring')
const CARGO = process.env.CARGO_BIN ?? 'cargo'

let baseUrl: string
let browser: Browser
let page: Page
let server: ReturnType<typeof Bun.spawn>
let workspace: string
let userRoot: string
const pageErrors: string[] = []
const warnings: string[] = []
const httpErrors: string[] = []

async function freePort(): Promise<number> {
  const probe = createServer()
  await new Promise<void>((resolve, reject) => {
    probe.once('error', reject)
    probe.listen(0, '127.0.0.1', resolve)
  })
  const address = probe.address()
  if (address === null || typeof address === 'string') throw new Error('failed to reserve a TCP port')
  await new Promise<void>((resolve, reject) => probe.close(error => error === undefined ? resolve() : reject(error)))
  return address.port
}

async function waitForServer(url: string): Promise<void> {
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    try {
      if ((await fetch(url)).ok) return
    } catch {}
    await Bun.sleep(100)
  }
  throw new Error(`native Tessivum server did not become ready at ${url}`)
}

function settingsDialog(): Locator {
  return page.getByRole('dialog', { name: '设置' })
}

function normalize(snapshot: string): string {
  return snapshot
    .replaceAll(userRoot, '{{presetRoot}}')
    .split('\n')
    .map(line => line.includes('alert: "the composition is not valid YAML:')
      ? `${line.slice(0, line.indexOf('alert:'))}alert: "{{brokenYaml}}"`
      : line)
    .join('\n')
    .trim()
}

async function expectGolden(locator: Locator, name: string): Promise<void> {
  expect(normalize(await locator.ariaSnapshot())).toBe((await Bun.file(join(SNAPSHOTS, name)).text()).trim())
}


beforeAll(async () => {
  workspace = await realpath(await mkdtemp(join(tmpdir(), 'tessivum-preset-authoring-')))
  userRoot = join(workspace, '.tessivum/.agent-presets')
  const build = Bun.spawn([CARGO, 'build', '--quiet', '--manifest-path', join(CRATE_ROOT, 'Cargo.toml'), '--bin', 'tessivum'], {
    cwd: CRATE_ROOT,
    stdout: 'inherit',
    stderr: 'inherit',
  })
  expect(await build.exited).toBe(0)

  const port = await freePort()
  baseUrl = `http://127.0.0.1:${port}`
  server = Bun.spawn([
    join(CRATE_ROOT, 'target/debug/tessivum'), 'web', '--data-dir', join(workspace, '.tessivum'),
  ], {
    cwd: workspace,
    env: {
      ...process.env,
      DEEPSEEK_API_KEY: 'test',
      TESSIVUM_AGENT_PRESET_ROOT: SHIPPED_PRESETS,
      TESSIVUM_WEB_ADDR: `127.0.0.1:${port}`,
    },
    stdout: 'inherit',
    stderr: 'inherit',
  })
  await waitForServer(baseUrl)

  browser = await chromium.launch(process.env.TESSIVUM_CHROMIUM === undefined
    ? { channel: 'chrome' }
    : { executablePath: process.env.TESSIVUM_CHROMIUM })
  page = await browser.newPage({ viewport: { width: 1680, height: 1000 }, locale: 'zh-CN' })
  page.on('pageerror', error => pageErrors.push(error.message))
  page.on('console', message => {
    if (message.type() === 'warning' || message.type() === 'error') warnings.push(message.text())
  })
  page.on('response', async response => {
    if (response.status() >= 400) {
      const body = await response.text().catch(() => '')
      httpErrors.push(`${response.status()} ${response.url()} ${response.request().postData() ?? ''} ${body}`)
    }
  })
  await page.goto(baseUrl, { waitUntil: 'load' })
  await page.getByRole('button', { name: '设置', exact: true }).waitFor({ timeout: 30_000 })
  const declaration = page.getByRole('dialog', { name: '内测声明' })
  if (await declaration.count() !== 0) await declaration.getByRole('button', { name: '继续' }).click()
  const credential = page.getByRole('dialog', { name: '添加一个 API Key 开始使用' })
  try {
    await credential.waitFor({ timeout: 3_000 })
    await credential.getByRole('button', { name: '稍后配置' }).click()
    await credential.waitFor({ state: 'hidden', timeout: 15_000 })
  } catch (error) {
    if (await credential.count() !== 0) throw error
  }
}, 120_000)

afterAll(async () => {
  await browser?.close()
  if (server !== undefined) {
    server.kill('SIGINT')
    await server.exited
  }
  if (workspace !== undefined) await rm(workspace, { force: true, recursive: true })
})

test('agent preset authoring is a host-side copy', async () => {
  await page.getByRole('button', { name: '设置', exact: true }).click()
  const settings = settingsDialog()
  await settings.getByRole('button', { name: 'Agent 预设' }).click()
  await settings.getByText('标准模式').first().waitFor({ timeout: 10_000 })
  await expectGolden(settings, 'section.expected.yml')

  await settings.getByRole('button', { name: '查看: 标准模式' }).click()
  const viewer = page.getByRole('dialog', { name: '查看 · 标准模式' })
  await viewer.waitFor()
  expect(await viewer.locator('pre').textContent()).toBe(await readFile(join(SHIPPED_PRESETS, 'standard/agent.cordis.yml'), 'utf8'))
  expect(await viewer.getByRole('textbox').count()).toBe(0)
  await viewer.getByRole('button', { name: '关闭' }).last().click()

  await settings.getByRole('button', { name: '复制: 极简模式' }).click()
  const copy = page.getByRole('dialog', { name: '复制预设 · 复制自 极简模式' })
  await expectGolden(copy, 'copy-dialog.expected.yml')
  await copy.getByPlaceholder('my-agent').fill('my-agent')
  await copy.getByPlaceholder('选择器中显示的名字，缺省用标识符').fill('我的模式')
  await copy.getByRole('button', { name: '创建' }).click()
  await copy.waitFor({ state: 'detached' })
  await settings.getByText('预设文件：').waitFor()
  await expectGolden(settings, 'created.expected.yml')
  expect(await readFile(join(userRoot, 'my-agent/agent.cordis.yml'), 'utf8'))
    .toBe(await readFile(join(SHIPPED_PRESETS, 'minimal/agent.cordis.yml'), 'utf8'))
  const metadata = await readFile(join(userRoot, 'my-agent/preset.yml'), 'utf8')
  expect(metadata).toContain('name: 我的模式')
  expect(metadata).toContain('description: 仅提供持久 bash 与 str_replace_editor 的双工具编码 Agent。')
  expect(metadata).not.toContain('order:')

  await settings.getByRole('button', { name: '删除: 我的模式' }).click()
  let confirm = page.getByRole('dialog', { name: '删除该预设？' })
  await confirm.getByRole('button', { name: '删除', exact: true }).click()
  await confirm.waitFor({ state: 'detached' })
  expect(existsSync(join(userRoot, 'my-agent'))).toBe(false)

  await mkdir(join(userRoot, 'broken-yaml'), { recursive: true })
  await writeFile(join(userRoot, 'broken-yaml/agent.cordis.yml'), '- id: x\n  name: [unclosed\n')
  await mkdir(join(userRoot, 'ghost'), { recursive: true })
  await writeFile(join(userRoot, 'ghost/preset.yml'), 'name: 幽灵预设\ndescription: composition 已被手动删除。\n')
  await settings.getByRole('button', { name: '通用设置' }).click()
  await settings.getByRole('button', { name: 'Agent 预设' }).click()
  await settings.getByText('加载失败').first().waitFor()
  await expectGolden(settings, 'damaged.expected.yml')
  expect(await settings.getByRole('button', { name: '加载失败: broken-yaml' }).isDisabled()).toBe(true)
  expect(await settings.getByRole('button', { name: '复制: 幽灵预设' }).isDisabled()).toBe(true)

  await settings.getByRole('button', { name: '删除: 幽灵预设' }).click()
  confirm = page.getByRole('dialog', { name: '删除该预设？' })
  await confirm.getByRole('button', { name: '删除', exact: true }).click()
  await confirm.waitFor({ state: 'detached' })
  expect(existsSync(join(userRoot, 'ghost'))).toBe(false)
  await settings.getByRole('button', { name: '复制: 极简模式' }).click()
  const reclaimed = page.getByRole('dialog', { name: '复制预设 · 复制自 极简模式' })
  await reclaimed.getByPlaceholder('my-agent').fill('ghost')
  await reclaimed.getByRole('button', { name: '创建' }).click()
  await reclaimed.waitFor({ state: 'detached' })
  await settings.getByRole('button', { name: '删除: ghost' }).click()
  confirm = page.getByRole('dialog', { name: '删除该预设？' })
  await confirm.getByRole('button', { name: '删除', exact: true }).click()
  await confirm.waitFor({ state: 'detached' })
  await rm(join(userRoot, 'broken-yaml'), { recursive: true })

  await settings.getByRole('button', { name: '关闭' }).last().click()
  await page.getByRole('button', { name: '设置', exact: true }).click()
  const reopened = settingsDialog()
  await reopened.getByRole('button', { name: 'Agent 预设' }).click()
  await reopened.getByRole('button', { name: '用「创造模式」创作自定义预设' }).click()
  await reopened.waitFor({ state: 'detached' })
  await page.getByRole('button', { name: '创造模式' }).waitFor({ timeout: 10_000 })
  await page.waitForFunction(async (url) => {
    const response = await fetch(`${url}/api/session.list`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ type: 'client-request', rpcId: 'creator', method: 'session.list', payload: {} }),
    })
    const body = await response.json() as { result?: { value?: { items?: { agentPreset?: string }[] } } }
    return body.result?.value?.items?.some(item => item.agentPreset === 'cordis') === true
  }, baseUrl, { timeout: 15_000 })
  const creatorResponse = await fetch(`${baseUrl}/api/session.list`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ type: 'client-request', rpcId: 'creator-check', method: 'session.list', payload: {} }),
  })
  const creatorBody = await creatorResponse.json() as { result: { value: { items: { agentPreset?: string }[] } } }
  expect(creatorBody.result.value.items.some(item => item.agentPreset === 'cordis')).toBe(true)

  expect(pageErrors).toEqual([])
  expect(warnings).toEqual([])
  expect(httpErrors).toEqual([])
}, 120_000)
