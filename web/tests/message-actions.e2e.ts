import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { afterAll, beforeAll, describe, expect, test } from 'bun:test'
import { captureStableAria, fixture, materializeRecording, openSessionByMarker, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/message-actions')

const SEED_ID = 'message-actions-web-e2e'
const PROMPT = 'Use the read tool twice in one assistant message: read a.txt and b.txt. Then reply with the single word DONE and stop.'
const MID_TURN_TEXT = 'I will read both files before answering.'
const SECOND_PROMPT = 'Now give the final answer.'

function completedTailFixture(raw: string): string {
  const kept: string[] = []
  for (const line of raw.trimEnd().split('\n')) {
    const row = JSON.parse(line) as {
      type: string
      agentPreset?: string
      seq?: number
      seq0?: number
      data?: { content?: unknown[] }
    }
    const firstSeq = row.seq ?? row.seq0
    if (firstSeq !== undefined && firstSeq >= 101) break
    if (row.type === 'session') {
      row.agentPreset = 'standard'
      kept.push(JSON.stringify(row))
      continue
    }
    if (row.type === 'assistant/message' && row.seq === 64) {
      const content = row.data?.content
      if (!Array.isArray(content)) throw new Error('borrowed step-one assistant message has no content')
      content.splice(1, 0, { type: 'text', text: MID_TURN_TEXT })
      kept.push(JSON.stringify(row))
    } else {
      kept.push(line)
    }
  }
  const tail = [
    { type: 'step/end', seq: 101, time: 1784974102749, data: { turn: 1, step: 2 } },
    { type: 'turn/end', seq: 102, time: 1784974102750, data: { turn: 1, reason: { kind: 'aborted' } } },
    { type: 'turn/start', seq: 103, time: 1784974103000, data: { turn: 2, trigger: { kind: 'message', source: { kind: 'user', rpcId: '{{rpcId}}' } } } },
    { type: 'user/message', seq: 104, time: 1784974103001, data: { content: [{ type: 'text', text: SECOND_PROMPT }], source: { kind: 'user', rpcId: '{{rpcId}}' } }, surfaceOp: 'append' },
    { type: 'step/start', seq: 105, time: 1784974103002, data: { turn: 2, step: 1 } },
    { type: 'assistant/message', seq: 106, time: 1784974103003, data: { turn: 2, step: 1, content: [{ type: 'text', text: 'DONE' }], provenance: { provider: 'deepseek-official', model: 'deepseek-v4-flash' } }, sourceEventSeqs: [], surfaceOp: 'append' },
    { type: 'step/end', seq: 107, time: 1784974103004, data: { turn: 2, step: 1 } },
    { type: 'turn/end', seq: 108, time: 1784974103005, data: { turn: 2, reason: { kind: 'completed' } } },
  ]
  return `${[...kept, ...tail.map(row => JSON.stringify(row))].join('\n')}\n`
}

async function fixtureUserPrompts(raw: string): Promise<string[]> {
  return raw.trim().split('\n')
    .map(line => JSON.parse(line) as { type?: string; data?: { content?: Array<{ type?: string; text?: string }> } })
    .filter(row => row.type === 'user/message')
    .flatMap(row => row.data?.content ?? [])
    .filter(block => block.type === 'text')
    .flatMap(block => block.text === undefined ? [] : [block.text])
}

async function expectGolden(harness: RustWebHarness, selector: string, name: string): Promise<void> {
  const actual = (await captureStableAria(harness.page, selector)).split(SEED_ID).join('{{seededId}}')
  expect(actual).toBe((await readFile(join(SNAPSHOT_DIR, name), 'utf8')).trim())
}

async function childCount(harness: RustWebHarness): Promise<number> {
  const response = await harness.rpc<{ items: Array<{ parentSessionId?: string }> }>('session.list')
  if (!response.ok || response.value === undefined) throw new Error(`session.list failed: ${JSON.stringify(response.error)}`)
  return response.value.items.filter(session => session.parentSessionId !== undefined).length
}

describe('message IconActions and clocks on settled history', () => {
  let harness: RustWebHarness

  beforeAll(async () => {
    harness = await RustWebHarness.launch({
      name: 'message-actions',
      beforeStart: async candidate => {
        await mkdir(candidate.workspace, { recursive: true })
        await writeFile(join(candidate.workspace, 'a.txt'), 'alpha\n')
        await writeFile(join(candidate.workspace, 'b.txt'), 'beta\n')
        const seed = materializeRecording(completedTailFixture(
          await readFile(await fixture('seeded-history', 'seed.jsonl'), 'utf8'),
        ))
        expect(await fixtureUserPrompts(seed)).toEqual([PROMPT, SECOND_PROMPT])
        await candidate.seedSession(SEED_ID, seed)
      },
    })
  }, 120_000)

  afterAll(async () => {
    await harness?.close()
  })

  test('enables branch only on the completed transcript tail', async () => {
    await openSessionByMarker(harness, PROMPT, 'DONE')
    await harness.page.getByRole('button', { name: 'Clear search', exact: true }).click()
    await harness.page.getByRole('tree', { name: 'Sessions' }).waitFor({ timeout: 10_000 })

    const copyButtons = harness.page.getByRole('button', { name: 'Copy' })
    await waitUntil(() => copyButtons.count(), count => count >= 4, 10_000)
    await copyButtons.first().focus()
    const branchButtons = harness.page.getByRole('button', { name: 'Branch into a new conversation' })
    await waitUntil(() => branchButtons.count(), count => count === 2, 5_000)
    await waitUntil(
      () => branchButtons.evaluateAll(buttons => buttons.map(button => button.getAttribute('aria-disabled'))),
      states => JSON.stringify(states) === JSON.stringify(['true', null]),
      5_000,
    )
    await branchButtons.first().focus()
    await waitUntil(() => harness.page.getByRole('tooltip').textContent(), text => text === 'Available only on the last message of a completed turn', 5_000)
    await waitUntil(() => harness.page.getByRole('button', { name: 'Edit' }).count(), count => count === 0, 5_000)
  }, 60_000)

  test('matches the conversation aria golden with IconActions and clocks', async () => {
    await harness.page.getByRole('button', { name: /^Select model, current/ }).waitFor({ timeout: 10_000 })
    await harness.page.getByText(/Cache hit \d+%/u).first().waitFor({ timeout: 10_000 })
    await harness.page.getByRole('button', { name: 'Copy' }).first().focus()
    await expectGolden(harness, '[class*="centerCol"]', 'ui.expected.md')
  })

  test('forks through the settled-message and session-row actions', async () => {
    const tree = harness.page.getByRole('tree', { name: 'Sessions' })
    const group = tree.getByRole('treeitem').first()
    if (await group.getAttribute('aria-expanded') === 'false') await group.click()
    await harness.page.getByRole('button', { name: 'Branch into a new conversation' }).last().click()
    await waitUntil(() => childCount(harness), count => count === 1, 15_000)
    await harness.page.getByRole('treeitem', { name: /Use the read tool twice \(1\)/ }).waitFor({ timeout: 10_000 })
    await waitUntil(() => harness.page.locator('[role="treeitem"][aria-selected="true"]').count(), count => count === 1, 10_000)

    const sourceRow = harness.page.locator('[role="treeitem"][aria-selected="true"]')
    const rowBox = await sourceRow.boundingBox()
    if (rowBox === null) throw new Error('fork source row has no layout box')
    const actionButton = sourceRow.locator('button[aria-label^="Session actions for "]')
    await sourceRow.hover({ position: { x: rowBox.width - 16, y: rowBox.height / 2 } })
    await waitUntil(() => actionButton.isVisible(), visible => visible, 2_000)
    const buttonBox = await actionButton.boundingBox()
    if (buttonBox === null) throw new Error('fork source row action has no layout box')
    await harness.page.mouse.click(buttonBox.x + buttonBox.width / 2, buttonBox.y + buttonBox.height / 2)
    await harness.page.getByRole('menuitem', { name: 'Fork session' }).click()
    await waitUntil(() => childCount(harness), count => count === 2, 15_000)
    await harness.page.getByRole('treeitem', { name: /Use the read tool twice \(2\)/ }).waitFor({ timeout: 10_000 })
    await waitUntil(() => harness.page.locator('[role="treeitem"][aria-selected="true"]').count(), count => count === 1, 10_000)
    await waitUntil(
      () => harness.page.locator('[role="treeitem"][aria-selected="true"]').textContent(),
      text => text?.includes('Use the read tool twice (2)') ?? false,
      10_000,
    )
    const listed = await harness.rpc<{ items: Array<{ parentSessionId?: string }> }>('session.list')
    expect(listed.ok).toBe(true)
    expect(listed.value?.items.filter(item => item.parentSessionId !== undefined)).toHaveLength(2)
    expect(listed.value?.items.some(item => item.parentSessionId === SEED_ID)).toBe(true)
    await expectGolden(harness, '[role="tree"][aria-label="Sessions"]', 'fork.expected.md')
  })

  test('issues zero model calls and keeps the fixture inventory closed', async () => {
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['fork.expected.md', 'ui.expected.md'])
    harness.assertClean()
  })
})
