import { afterAll, beforeAll, expect, test } from 'bun:test'
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { chromium, type Browser, type Page } from 'playwright-core'

const CRATE_ROOT = join(dirname(fileURLToPath(import.meta.url)), '../..')
const CARGO = process.env.CARGO_BIN ?? 'cargo'
const GOLDEN = join(dirname(fileURLToPath(import.meta.url)), 'snapshots/access-confirmation/ui.expected.yml')

let browser: Browser
let page: Page
let server: ReturnType<typeof Bun.spawn>
let workspace: string
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

beforeAll(async () => {
  workspace = await mkdtemp(join(tmpdir(), 'tessivum-access-confirmation-'))
  await mkdir(join(workspace, '.tessivum'))
  await writeFile(join(workspace, '.tessivum/settings.yaml'), [
    'llm-pi-ai:',
    '  providers:',
    '    deepseek-official:',
    '      displayName: DeepSeek',
    '      baseURL: https://api.deepseek.com/v1',
    '      apiKeyEnv: DEEPSEEK_API_KEY',
    '      models: [{ id: deepseek-v4-flash, name: DeepSeek V4 Flash }]',
    'agent-default-model:',
    '  provider: deepseek-official',
    '  model: deepseek-v4-flash',
    '',
  ].join('\n'))
  const build = Bun.spawn([CARGO, 'build', '--quiet', '--manifest-path', join(CRATE_ROOT, 'Cargo.toml'), '--bin', 'tessivum'], {
    cwd: CRATE_ROOT,
    stdout: 'inherit',
    stderr: 'inherit',
  })
  expect(await build.exited).toBe(0)

  const port = await freePort()
  const baseUrl = `http://127.0.0.1:${port}`
  server = Bun.spawn([
    join(CRATE_ROOT, 'target/debug/tessivum'), 'web', '--data-dir', join(workspace, '.tessivum'),
  ], {
    cwd: workspace,
    env: { ...process.env, DEEPSEEK_API_KEY: 'test', TESSIVUM_WEB_ADDR: `127.0.0.1:${port}` },
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
  await page.locator('button[aria-label^="访问模式"]').first().waitFor({ timeout: 30_000 })
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
  const createResult = await fetch(`${baseUrl}/api/session.create`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ type: 'client-request', rpcId: 'access-session', method: 'session.create', payload: {} }),
  }).then(response => response.json()) as { result: { ok: boolean; value?: { sessionId: string } } }
  expect(createResult.result.ok).toBe(true)
  const sessionId = createResult.result.value?.sessionId
  if (sessionId === undefined) throw new Error('session.create returned no sessionId')
  const commandResult = await fetch(`${baseUrl}/api/commands/execute`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ type: 'client-request', rpcId: 'access-command', method: 'commands/execute', payload: { args: { agentId: sessionId, line: '/permission workspace-write' } } }),
  }).then(response => response.json()) as { result: { ok: boolean; error?: unknown } }
  if (!commandResult.result.ok) throw new Error(JSON.stringify(commandResult.result.error))
  await page.getByRole('treeitem').last().click()
}, 120_000)

afterAll(async () => {
  await browser?.close()
  if (server !== undefined) {
    server.kill('SIGINT')
    await server.exited
  }
  if (workspace !== undefined) await rm(workspace, { force: true, recursive: true })
})

test('Full access requires an in-page risk acknowledgement', async () => {
  const access = page.locator('button[aria-label^="访问模式"]').first()
  expect(await access.getAttribute('aria-label')).toBe('访问模式，当前：Workspace Write')

  await access.click()
  await page.getByRole('menuitem', { name: 'Full access' }).click()
  const dialog = page.getByRole('dialog', { name: '确认启用 Full access？' })
  await dialog.waitFor({ timeout: 10_000 })
  const enable = dialog.getByRole('button', { name: '启用 Full access' })
  expect(await enable.isDisabled()).toBe(true)
  expect(await dialog.evaluate(node => node.parentElement?.parentElement === document.body)).toBe(true)
  expect((await dialog.ariaSnapshot()).trim()).toBe((await Bun.file(GOLDEN).text()).trim())

  await dialog.getByRole('checkbox', { name: '我已了解风险，并愿意继续' }).check()
  expect(await enable.isEnabled()).toBe(true)
  await enable.click()
  await page.waitForFunction(() => document.querySelector('button[aria-label="访问模式，当前：Full access"]') !== null)
  expect(await dialog.count()).toBe(0)
  expect(pageErrors).toEqual([])
  expect(httpErrors).toEqual([])
  expect(warnings).toEqual([])
}, 60_000)
