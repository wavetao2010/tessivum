import { afterAll, beforeAll, describe, expect, test } from 'bun:test'
import { mkdir, mkdtemp, realpath, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { createServer } from 'node:net'
import { chromium, type Browser, type Locator, type Page } from 'playwright-core'

const HERE = dirname(import.meta.path)
const CRATE_ROOT = join(HERE, '../..')
const SHIPPED_PRESETS = join(process.env.TESSIVUM_DEEPSEEK_SOURCE ?? join(CRATE_ROOT, '../upstream/deepseek-harness'), 'apps/cli/config/agent-presets')
const SNAPSHOT_DIR = join(HERE, 'snapshots/agent-preset-selection')
const CARGO = process.env.CARGO_BIN ?? 'cargo'
const SEED_ID = 'agent-preset-selection-web-e2e'
const CHILD_ID = 'agent-preset-selection-child'
const SKILL_NAME = 'preset-catalog-demo'

let workspace: string
let fixtureRoot: string
let project: string
let userRoot: string
let baseUrl: string
let server: Bun.Subprocess
let browser: Browser
let page: Page
let warnings: string[]
let pageErrors: string[]

function normalize(snapshot: string): string {
  return snapshot.replaceAll(workspace, '<workspace>').replaceAll('\\', '/').trim()
}

async function expectGolden(locator: Locator, name: string): Promise<void> {
  expect(normalize(await locator.ariaSnapshot())).toBe(
    normalize(await Bun.file(join(SNAPSHOT_DIR, name)).text()),
  )
}

function sessionPath(root: string, id: string): string {
  return join(root, `session-${Buffer.from(id).toString('hex')}.jsonl`)
}

async function seedSessions(root: string): Promise<void> {
  await mkdir(root, { recursive: true })
  const createdAt = 1_784_974_100_000
  const parent = [
    { type: 'session', version: 0, id: SEED_ID, createdAt, cwd: join(workspace, 'seeded-root'), delegationDepth: 0, agentPreset: 'minimal' },
    { type: 'turn/start', seq: 0, time: createdAt, data: { turn: 1, trigger: { kind: 'message', source: { kind: 'user', rpcId: 'seed' } } } },
    { type: 'user/message', seq: 1, time: createdAt + 1, data: { content: [{ type: 'text', text: 'Seeded turn.' }], source: { kind: 'user', rpcId: 'seed' } }, surfaceOp: 'append' },
    { type: 'session/title', seq: 2, time: createdAt + 2, data: { title: 'Seeded turn', messageSeqs: [1], source: { kind: 'fallback' } } },
    { type: 'turn/end', seq: 3, time: createdAt + 3, data: { turn: 1, reason: { kind: 'completed' } } },
  ]
  const child = [
    {
      type: 'session', version: 0, id: CHILD_ID, createdAt: createdAt + 100,
      cwd: workspace, parentSession: SEED_ID, origin: 'subagent', delegationDepth: 1,
      agentPreset: 'minimal',
    },
    { type: 'turn/start', seq: 0, time: createdAt + 101, data: { turn: 1, trigger: { kind: 'subagent', source: { kind: 'subagent', parentSessionId: SEED_ID } } } },
    { type: 'turn/end', seq: 1, time: createdAt + 102, data: { turn: 1, reason: { kind: 'completed' } } },
  ]
  await writeFile(sessionPath(root, SEED_ID), `${parent.map(row => JSON.stringify(row)).join('\n')}\n`)
  await writeFile(sessionPath(root, CHILD_ID), `${child.map(row => JSON.stringify(row)).join('\n')}\n`)
}

async function freePort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const socket = createServer()
    socket.once('error', reject)
    socket.listen(0, '127.0.0.1', () => {
      const address = socket.address()
      if (typeof address === 'string' || address === null) return reject(new Error('missing port'))
      socket.close(error => error ? reject(error) : resolve(address.port))
    })
  })
}

async function waitForServer(url: string): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt++) {
    try {
      if ((await fetch(url)).ok) return
    } catch {}
    await Bun.sleep(50)
  }
  throw new Error('server did not start')
}

async function waitUntil<T>(read: () => Promise<T>, accepts: (value: T) => boolean): Promise<T> {
  const deadline = Date.now() + 15_000
  while (Date.now() < deadline) {
    const value = await read()
    if (accepts(value)) return value
    await Bun.sleep(50)
  }
  throw new Error('condition did not become true')
}

async function connectWorkspace(): Promise<void> {
  await page.locator('textarea:enabled[placeholder="Describe what you want to build"]').waitFor()
}

interface RpcItem {
  [key: string]: unknown
  agentPreset?: unknown
  cwd?: unknown
  path?: unknown
  sessionId?: unknown
  workspaceId?: unknown
}

interface RpcResponse {
  result: { ok: boolean; value?: { items?: RpcItem[]; sessionId?: string } }
}

async function rpc(method: string, payload: Record<string, unknown>): Promise<RpcResponse> {
  const response = await fetch(`${baseUrl}/api/${method}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ type: 'client-request', rpcId: `${method}-${crypto.randomUUID()}`, method, payload }),
  })
  const body: unknown = await response.json()
  if (typeof body !== 'object' || body === null || !('result' in body)
    || typeof body.result !== 'object' || body.result === null || !('ok' in body.result)
    || typeof body.result.ok !== 'boolean') {
    throw new Error(`${method} returned an invalid response`)
  }
  const value = 'value' in body.result ? body.result.value : undefined
  if (value !== undefined && (typeof value !== 'object' || value === null)) {
    throw new Error(`${method} returned an invalid value`)
  }
  return body as RpcResponse
}

async function openLiveSession(): Promise<void> {
  await page.getByRole('treeitem', { name: 'workspace', exact: true }).hover()
  await page.getByRole('button', { name: 'New session in workspace' }).click()
  await waitUntil(livePreset, preset => preset !== undefined)
}

async function livePreset(): Promise<string | null | undefined> {
  const body = await rpc('session.list', {})
  const entry = body.result.value?.items?.find(item =>
    item.sessionId !== SEED_ID && item.sessionId !== CHILD_ID && item.cwd === project,
  )
  if (entry === undefined) return undefined
  return typeof entry.agentPreset === 'string' ? entry.agentPreset : null
}

async function menuOptions(): Promise<string[]> {
  const menu = page.getByRole('listbox', { name: 'Trigger suggestions' })
  await menu.waitFor({ timeout: 15_000 })
  return await menu.getByRole('option').allTextContents()
}

beforeAll(async () => {
  fixtureRoot = await realpath(await mkdtemp(join(tmpdir(), 'tessivum-preset-selection-')))
  await mkdir(join(fixtureRoot, 'workspace'))
  workspace = await realpath(join(fixtureRoot, 'workspace'))
  project = workspace
  userRoot = join(workspace, 'home')
  const data = join(workspace, '.tessivum')
  await mkdir(join(project, '.agents', 'skills', SKILL_NAME), { recursive: true })
  await writeFile(join(project, '.agents', 'skills', SKILL_NAME, 'SKILL.md'), [
    '---',
    `name: ${SKILL_NAME}`,
    'description: Prove the slash catalog follows the session composition',
    '---',
    '',
    'Body.',
    '',
  ].join('\n'))
  await seedSessions(data)
  await mkdir(userRoot, { recursive: true })

  const build = Bun.spawn([CARGO, 'build', '--quiet', '--manifest-path', join(CRATE_ROOT, 'Cargo.toml'), '--bin', 'tessivum'], {
    cwd: CRATE_ROOT,
    stdout: 'inherit',
    stderr: 'inherit',
  })
  expect(await build.exited).toBe(0)
  const port = await freePort()
  baseUrl = `http://127.0.0.1:${port}`
  server = Bun.spawn([join(CRATE_ROOT, 'target/debug/tessivum'), 'web'], {
    cwd: workspace,
    env: {
      ...process.env,
      DEEPSEEK_API_KEY: 'test',
      HOME: userRoot,
      TESSIVUM_AGENT_PRESET_ROOT: SHIPPED_PRESETS,
      TESSIVUM_WEB_ADDR: `127.0.0.1:${port}`,
      TESSIVUM_AGENT_PRESET_USER_ROOT: join(userRoot, '.agent-presets'),
    },
    stdout: 'inherit',
    stderr: 'inherit',
  })
  await waitForServer(baseUrl)

  browser = await chromium.launch(process.env.TESSIVUM_CHROMIUM === undefined
    ? { channel: 'chrome' }
    : { executablePath: process.env.TESSIVUM_CHROMIUM })
  page = await browser.newPage({ locale: 'en-US' })
  warnings = []
  pageErrors = []
  page.on('console', message => {
    if (message.type() === 'warning' || message.type() === 'error') warnings.push(message.text())
  })
  page.on('pageerror', error => pageErrors.push(error.message))
  await page.goto(baseUrl, { waitUntil: 'load' })
  await page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
  const declaration = page.getByRole('dialog', { name: 'Internal Testing Notice' })
  if (await declaration.count() !== 0) {
    await declaration.getByRole('button', { name: 'Continue' }).click()
    await declaration.waitFor({ state: 'detached' })
  }
  await connectWorkspace()
}, 120_000)
afterAll(async () => {
  await browser?.close()
  server?.kill()
  await server?.exited
  if (fixtureRoot) await rm(fixtureRoot, { recursive: true, force: true })
})

describe('agent preset selection follows the host composition', () => {
  test('offers the deployment roster on the new-session screen', async () => {
    const row = page.locator('[class*="heroWorkspaceRow"]')
    await expectGolden(row, 'hero.expected.yml')
    await page.getByRole('button', { name: 'Standard mode' }).click()
    const menu = page.getByRole('menu')
    await menu.waitFor()
    await expectGolden(menu, 'menu.expected.yml')
    await page.keyboard.press('Escape')
  })

  test('switches the live preset and re-reads its slash catalog', async () => {
    await openLiveSession()
    await page.getByRole('button', { name: 'Standard mode' }).click()
    await page.getByRole('menuitem', { name: /Minimal mode/ }).click()
    await waitUntil(livePreset, preset => preset === 'minimal')

    const composer = page.locator('textarea:enabled').last()
    await composer.fill('/')
    await waitUntil(async () => {
      await composer.fill('')
      await composer.fill('/')
      return await menuOptions()
    }, options => !options.some(option =>
      option.includes(SKILL_NAME) || option.startsWith('compact') || option.startsWith('plan')))
    const minimal = await menuOptions()
    expect(minimal.some(option => option.startsWith('compact'))).toBe(false)
    expect(minimal.some(option => option.startsWith('plan'))).toBe(false)
    expect(minimal.some(option => option.startsWith('goal'))).toBe(true)
    expect(minimal.some(option => option.startsWith('model'))).toBe(true)
    await composer.fill('')

    await page.getByRole('button', { name: 'Minimal mode' }).click()
    await page.getByRole('menuitem', { name: /^Standard mode/ }).first().click()
    await waitUntil(livePreset, preset => preset === 'standard')
    await composer.fill('/')
    await waitUntil(menuOptions, options => options.some(option => option.includes(SKILL_NAME)))
    const standard = await menuOptions()
    expect(standard.some(option => option.startsWith('compact'))).toBe(true)
    expect(standard.some(option => option.startsWith('plan'))).toBe(true)
    await composer.fill('')
  }, 90_000)

  test('restores a session preset as static header chrome', async () => {
    await page.getByRole('treeitem', { name: /^Ungrouped/ }).click()
    await page.getByRole('treeitem', { name: /^Seeded turn/ }).click()
    await page.getByText('Seeded turn.').waitFor({ timeout: 15_000 })
    const title = page.locator('[class*="titleRow"]')
    await expectGolden(title, 'header.expected.yml')
    const snapshot = await title.ariaSnapshot()
    expect(snapshot.indexOf('Minimal mode')).toBeLessThan(snapshot.indexOf('button "1 subagent"'))
    expect(snapshot.indexOf('button "1 subagent"')).toBeLessThan(snapshot.indexOf('button "Session log"'))
    expect(snapshot).not.toContain('button "Minimal mode"')
  })

  test('has no browser errors or stream warnings', () => {
    expect(pageErrors).toEqual([])
    expect(warnings).toEqual([])
  })
})
