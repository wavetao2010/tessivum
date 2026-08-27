import { afterAll, beforeAll, describe, expect, test } from 'bun:test'
import { mkdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import type { Locator } from 'playwright-core'
import { RustWebHarness } from './support.ts'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/agent-mode-selection')
const SEED_ID = 'agent-mode-selection-web-e2e'
const CHILD_ID = 'agent-mode-selection-child'

let harness: RustWebHarness

function normalize(snapshot: string): string {
  return snapshot.replaceAll(harness.workspace, '<workspace>').replaceAll('\\', '/').trim()
}

async function expectGolden(locator: Locator, name: string): Promise<void> {
  expect(normalize(await locator.ariaSnapshot())).toBe(
    normalize(await Bun.file(join(SNAPSHOT_DIR, name)).text()),
  )
}


async function seedSessions(root: string, workspace: string): Promise<void> {
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
  await writeFile(join(root, `session-${Buffer.from(SEED_ID).toString('hex')}.jsonl`), `${parent.map(row => JSON.stringify(row)).join('\n')}\n`)
  await writeFile(join(root, `session-${Buffer.from(CHILD_ID).toString('hex')}.jsonl`), `${child.map(row => JSON.stringify(row)).join('\n')}\n`)
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

interface SessionWireItem {
  sessionId: string
  cwd?: string
  agentPreset?: string
}

async function liveMode(): Promise<string | undefined> {
  const result = await harness.rpc<{ items: SessionWireItem[] }>('session.list')
  if (!result.ok || result.value === undefined) throw new Error(`session.list failed: ${JSON.stringify(result.error)}`)
  return result.value.items.find(item =>
    item.sessionId !== SEED_ID && item.sessionId !== CHILD_ID && item.cwd === harness.workspace,
  )?.agentPreset
}

async function openLiveSession(): Promise<void> {
  await harness.page.getByRole('treeitem', { name: 'workspace', exact: true }).hover()
  await harness.page.getByRole('button', { name: 'New session in workspace' }).click()
  await waitUntil(liveMode, mode => mode !== undefined)
}

beforeAll(async () => {
  harness = await RustWebHarness.launch({
    name: 'agent-mode-selection',
    locale: 'en-US',
    beforeStart: async instance => { await seedSessions(instance.dataDir, instance.workspace) },
  })
  await harness.page.locator('textarea:enabled[placeholder="Describe what you want to build"]').waitFor()
}, 120_000)

afterAll(async () => { await harness?.close() })

describe('Agent Mode selection follows the Host roster', () => {
  test('offers all four Native Agent Modes on the new-session screen', async () => {
    const row = harness.page.locator('[class*="heroWorkspaceRow"]')
    await expectGolden(row, 'hero.expected.yml')
    await harness.page.getByRole('button', { name: 'Standard' }).click()
    const menu = harness.page.getByRole('menu')
    await menu.waitFor()
    await expectGolden(menu, 'menu.expected.yml')
    for (const name of ['Standard', 'PTC', 'Minimal', 'Composition']) {
      expect(await menu.getByRole('menuitem', { name: new RegExp(`^${name}`) }).count()).toBe(1)
    }
    await harness.page.keyboard.press('Escape')
  })

  test('sends each selected Native mode ID through the compatibility wire', async () => {
    await openLiveSession()
    let current = 'Standard'
    for (const [name, id] of [
      ['PTC', 'ptc'],
      ['Minimal', 'minimal'],
      ['Composition', 'composition'],
      ['Standard', 'standard'],
    ] as const) {
      await harness.page.getByRole('button', { name: current, exact: true }).click()
      await harness.page.getByRole('menuitem', { name: new RegExp(`^${name}`) }).click()
      await waitUntil(liveMode, mode => mode === id)
      current = name
    }
  }, 90_000)

  test('projects the compatibility agentPreset field as static Agent Mode header chrome', async () => {
    await harness.page.getByRole('treeitem', { name: /^Ungrouped/ }).click()
    await harness.page.getByRole('treeitem', { name: /^Seeded turn/ }).click()
    await harness.page.getByText('Seeded turn.').waitFor({ timeout: 15_000 })
    const title = harness.page.locator('[class*="titleRow"]')
    await expectGolden(title, 'header.expected.yml')
    const snapshot = await title.ariaSnapshot()
    expect(snapshot.indexOf('Minimal')).toBeLessThan(snapshot.indexOf('button "1 subagent"'))
    expect(snapshot.indexOf('button "1 subagent"')).toBeLessThan(snapshot.indexOf('button "Session log"'))
    expect(snapshot).not.toContain('button "Minimal"')
  })

  test('has no browser errors or stream warnings', () => { harness.assertClean() })
})
