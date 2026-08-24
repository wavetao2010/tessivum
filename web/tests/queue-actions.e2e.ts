import { existsSync } from 'node:fs'
import { mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, stableAria, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/queue-actions')
const FIXTURE = join(import.meta.dir, 'snapshots/live-interactions/session.jsonl')
const COLLAPSED_EXPECTED = join(SNAPSHOT_DIR, 'collapsed.expected.md')
const EDITING_EXPECTED = join(SNAPSHOT_DIR, 'editing.expected.md')
const LAYOUT_EXPECTED = join(SNAPSHOT_DIR, 'layout.expected.md')
const PRESERVED_EXPECTED = join(SNAPSHOT_DIR, 'preserved.expected.md')
const UI_EXPECTED = join(SNAPSHOT_DIR, 'ui.expected.md')
const ACTIVE_PROMPT = 'Reply with a one-sentence description of event sourcing, then stop.'
const REMOVE = 'Queue item to remove'
const EDIT = 'Queue item to edit'
const EDITED = 'Edited queue item'
const TAIL = 'Queue item preserved after stop'
const WAKE = 'Wake the preserved queue'

type ObjectValue = Record<string, unknown>
type Event = { type: string; data: ObjectValue }

function isObject(value: unknown): value is ObjectValue {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function parseEvent(line: string): Event {
  const value: unknown = JSON.parse(line)
  if (!isObject(value) || typeof value.type !== 'string' || !isObject(value.data)) {
    throw new Error('durable event is malformed')
  }
  return { type: value.type, data: value.data }
}

function textBlocks(value: unknown): string[] {
  return Array.isArray(value) ? value.flatMap((block) => {
    if (!isObject(block) || block.type !== 'text' || typeof block.text !== 'string') return []
    return [block.text]
  }) : []
}

function normalizeAria(snapshot: string, workspace: string): string {
  const base = workspace.split('/').at(-1) ?? workspace
  return stableAria(snapshot)
    .split(workspace).join('{{cwd}}')
    .split(base).join('{{workspace}}')
    .replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, '{{uuid}}')
    .replace(/~\d+(?:y(?: \d+mo)?|mo(?: \d+d)?)|\b(?:\d+d(?: \d+h(?: \d+m \d+s)?)?|\d+h \d+m \d+s|\d+m ?\d+s|\d+(?:\.\d+)?s|\d+(?:\.\d+)?ms)\b/g, duration => duration.startsWith('~') ? duration : '{{duration}}')
    .replace(/约\d+(?:年(?:\d+个月)?|个月(?:\d+天)?)|\d+(?:天(?:\d+小时(?:\d+分\d+秒)?)?|小时\d+分\d+秒|分\d+秒|(?:\.\d+)?秒)/g, duration => duration.startsWith('约') ? duration : '{{duration}}')
    .replace(/\d+(?:\.\d+)?(?= tok\/s(?!\w))/g, '{{throughput}}')
    .replace(/(Compacted \d+ history items \(~)\d+( tokens\))/g, '$1{{tokens}}$2')
    .replace(/\d{4}年\d{1,2}月\d{1,2}日 \d{2}:\d{2}/g, '{{clock}}')
    .replace(/\d{1,2}月\d{1,2}日 \d{2}:\d{2}/g, '{{clock}}')
    .replace(/(?<!\d)\d{1,2}:\d{2}:\d{2}(?:\.\d+)?(?:\s*[AP]M)?(?!\d)/gi, '{{clock}}')
    .replace(/(?<!\d)\d{2}:\d{2}(?!\d)/g, '{{clock}}')
}

async function captureStableAria(harness: RustWebHarness, selector: string): Promise<string> {
  const region = harness.page.locator(selector).first()
  let previous = normalizeAria(await region.ariaSnapshot(), harness.workspace)
  await waitUntil(async () => {
    const current = normalizeAria(await region.ariaSnapshot(), harness.workspace)
    const stable = current === previous
    previous = current
    return stable
  }, Boolean, 5_000)
  return previous
}

async function sessionEvents(harness: RustWebHarness, sessionId: string): Promise<Event[]> {
  const file = join(harness.dataDir, `session-${Buffer.from(sessionId).toString('hex')}.jsonl`)
  return (await readFile(file, 'utf8')).trim().split('\n').slice(1).map(parseEvent)
}

function recordedChunks(recording: string): unknown[] {
  return recording.trim().split('\n').slice(1).flatMap((line) => {
    const event = JSON.parse(line) as { type?: string; data?: { chunk?: unknown; index?: number; texts?: unknown } }
    if (event.type === 'assistant/chunk' && event.data?.chunk !== undefined) return [event.data.chunk]
    const type = event.type === 'reasoning-chunks' ? 'reasoning-delta'
      : event.type === 'text-chunks' ? 'text-delta' : undefined
    if (type === undefined || typeof event.data?.index !== 'number' || !Array.isArray(event.data.texts)) return []
    return event.data.texts.flatMap(text => typeof text === 'string' ? [{ type, index: event.data!.index, text }] : [])
  })
}

async function writeQueueOverride(path: string, readyFile: string, repeats: number): Promise<void> {
  const chunks = recordedChunks(await readFile(FIXTURE, 'utf8'))
  await writeFile(path, JSON.stringify([
    { kind: 'hang', readyFile },
    ...Array.from({ length: repeats }, () => ({ kind: 'chunks', chunks })),
  ]))
}

function turnEndReasons(log: readonly Event[]): string[] {
  return log.flatMap((event) => {
    const kind = isObject(event.data.reason) ? event.data.reason.kind : undefined
    return event.type === 'turn/end' && typeof kind === 'string' ? [kind] : []
  })
}

function userTexts(log: readonly Event[]): string[] {
  return log.flatMap((event) => event.type === 'user/message'
    && isObject(event.data.source) && event.data.source.kind === 'user'
    ? textBlocks(event.data.content) : [])
}

test('edits and removes exact occurrences and preserves Queue across stop', async () => {
  const overrideDir = await mkdtemp(join(tmpdir(), 'tessivum-web-queue-actions-'))
  const readyFile = join(overrideDir, '.hang-ready')
  const overridePath = join(overrideDir, 'replay.override.json')
  await writeQueueOverride(overridePath, readyFile, 3)
  const harness = await RustWebHarness.launch({
    name: 'queue-actions-web-e2e', locale: 'en-US', replayFixture: FIXTURE, replayOverride: overridePath,
  })
  try {
    const input = harness.page.locator('textarea').first()
    const firstSettled = harness.whenTurnSettled()
    await input.fill(ACTIVE_PROMPT)
    await input.press('Enter')
    await expect(waitUntil(async () => existsSync(readyFile), Boolean)).resolves.toBe(true)

    for (const text of [REMOVE, EDIT]) {
      await input.fill(text)
      await input.press('Enter')
    }
    const queueHeader = harness.page.getByRole('button', { name: '2 queued messages' })
    await expect(waitUntil(() => queueHeader.getAttribute('aria-expanded'), value => value === 'false')).resolves.toBe('false')
    expect(`${await captureStableAria(harness, '[class*="centerCol"]')}\n`).toBe(await readFile(COLLAPSED_EXPECTED, 'utf8'))
    await queueHeader.click()
    await expect(waitUntil(() => harness.page.getByRole('button', { name: 'Remove queued message' }).count(), count => count === 2)).resolves.toBe(2)

    await harness.page.setViewportSize({ width: 640, height: 1000 })
    const [queueBox, composerBox] = await waitUntil(
      () => Promise.all([
        harness.page.locator('[data-queue-dock]').boundingBox(),
        harness.page.locator('[data-composer-card]').boundingBox(),
      ]),
      ([queue, composer]) => {
        if (queue === null || composer === null) return false
        const left = queue.x - composer.x
        const right = composer.x + composer.width - queue.x - queue.width
        return Math.abs(left - right) < 0.05
      },
    )
    expect(queueBox).not.toBeNull()
    expect(composerBox).not.toBeNull()
    expect(queueBox!.x).toBeGreaterThanOrEqual(composerBox!.x)
    expect(queueBox!.x + queueBox!.width).toBeLessThanOrEqual(composerBox!.x + composerBox!.width)
    const metrics = await harness.page.locator('[data-composer-card]').evaluate((element) => Number.parseFloat(
      getComputedStyle(element).getPropertyValue('--dsh-composer-dock-inset'),
    ))
    const leftInset = queueBox!.x - composerBox!.x
    const rightInset = composerBox!.x + composerBox!.width - queueBox!.x - queueBox!.width
    expect(leftInset).toBeGreaterThanOrEqual(metrics)
    expect(rightInset).toBeCloseTo(leftInset, 1)
    await harness.page.setViewportSize({ width: 1680, height: 1000 })

    const editRow = harness.page.getByText(EDIT, { exact: true }).locator('..')
    await editRow.getByRole('button', { name: 'Edit queued message' }).click()
    const editor = harness.page.getByRole('textbox', { name: 'Edit queued message' })
    await editor.fill(EDITED)
    expect(`${await captureStableAria(harness, '[class*="centerCol"]')}\n`).toBe(await readFile(EDITING_EXPECTED, 'utf8'))
    await harness.page.getByRole('button', { name: 'Save queued message' }).click()
    await harness.page.getByText(EDITED, { exact: true }).waitFor()

    const removeRow = harness.page.getByText(REMOVE, { exact: true }).locator('..')
    await removeRow.getByRole('button', { name: 'Remove queued message' }).click()
    await expect(waitUntil(() => harness.page.getByText(REMOVE, { exact: true }).count(), count => count === 0)).resolves.toBe(0)
    expect(`${await captureStableAria(harness, '[class*="centerCol"]')}\n`).toBe(await readFile(UI_EXPECTED, 'utf8'))

    const active = (await harness.sessions()).find(item => item.running)
    if (active === undefined) throw new Error('queue scenario has no active session')
    expect(userTexts(await sessionEvents(harness, active.sessionId))).toEqual([ACTIVE_PROMPT])
    harness.assertClean()

    await input.fill(TAIL)
    await input.press('Enter')
    await expect(waitUntil(() => harness.page.getByRole('button', { name: 'Remove queued message' }).count(), count => count === 2)).resolves.toBe(2)
    await harness.page.getByRole('button', { name: 'Stop generating' }).click()
    const sessionId = await firstSettled
    await expect(waitUntil(() => harness.page.getByRole('button', { name: 'Stop generating' }).count(), count => count === 0)).resolves.toBe(0)
    await expect(waitUntil(() => harness.page.getByRole('button', { name: 'Remove queued message' }).count(), count => count === 2)).resolves.toBe(2)
    expect(`${await captureStableAria(harness, '[class*="centerCol"]')}\n`).toBe(await readFile(PRESERVED_EXPECTED, 'utf8'))

    const settled = harness.whenTurnSettled()
    await input.fill(WAKE)
    await input.press('Enter')
    await settled
    const log = await sessionEvents(harness, sessionId)
    await expect(waitUntil(async () => turnEndReasons(await sessionEvents(harness, sessionId)), reasons => reasons.length === 4)).resolves.toEqual(['aborted', 'completed', 'completed', 'completed'])
    expect(userTexts(log)).toEqual([ACTIVE_PROMPT, EDITED, TAIL, WAKE])
    await expect(waitUntil(() => harness.page.locator('[data-queue-dock]').count(), count => count === 0)).resolves.toBe(0)
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
      'collapsed.expected.md', 'editing.expected.md', 'layout.expected.md', 'preserved.expected.md', 'ui.expected.md',
    ])
    harness.assertClean()
  } finally {
    await harness.close()
    await rm(overrideDir, { recursive: true, force: true })
  }
}, 120_000)

test('orders Todo before Goal and Queue on one responsive card column', async () => {
  const overrideDir = await mkdtemp(join(tmpdir(), 'tessivum-web-context-layout-'))
  const readyFile = join(overrideDir, '.hang-ready')
  const overridePath = join(overrideDir, 'replay.override.json')
  const todos = [
    { content: 'Confirm the panel order', status: 'completed' },
    { content: 'Align the panel widths', status: 'in_progress' },
  ]
  const argumentsJson = JSON.stringify({ todos })
  await writeFile(overridePath, JSON.stringify([
    { kind: 'chunks', chunks: [
      { type: 'block-start', index: 0, blockType: 'tool-call' },
      { type: 'tool-call-delta', index: 0, id: 'layout-todo', name: 'todo_write', argumentsDelta: argumentsJson },
      { type: 'block-end', index: 0, block: { type: 'tool-call', id: 'layout-todo', name: 'todo_write', arguments: argumentsJson } },
      { type: 'usage', usage: { inputTokens: 10, outputTokens: 10 } },
      { type: 'finish', reason: { kind: 'tool-calls' } },
    ] },
    { kind: 'hang', readyFile },
  ]))
  const harness = await RustWebHarness.launch({
    name: 'queue-actions-layout-web-e2e', locale: 'en-US', replayFixture: FIXTURE, replayOverride: overridePath,
  })
  try {
    const input = harness.page.locator('textarea').first()
    await input.fill('/goal Keep the composer context panels aligned')
    await input.press('Enter')
    await expect(waitUntil(async () => existsSync(readyFile), Boolean, 15_000)).resolves.toBe(true)
    await harness.page.locator('[data-goal-bar]').waitFor({ timeout: 10_000 })
    await harness.page.locator('[data-testid="todo-panel"]').waitFor({ timeout: 10_000 })
    const sessionId = (await harness.sessions()).find(item => item.running)?.sessionId
    if (sessionId === undefined) throw new Error('layout scenario has no active session')

    for (const text of ['Layout queue first', 'Layout queue second']) {
      await input.fill(text)
      await input.press('Enter')
    }
    const queueHeader = harness.page.getByRole('button', { name: '2 queued messages' })
    await expect(waitUntil(() => queueHeader.getAttribute('aria-expanded'), value => value === 'false')).resolves.toBe('false')
    expect(`${await captureStableAria(harness, '[class*="centerCol"]')}\n`).toBe(await readFile(LAYOUT_EXPECTED, 'utf8'))

    const aligned = async (): Promise<void> => {
      const [queueBox, todoBox, goalBox] = await waitUntil(
        () => Promise.all([
          harness.page.locator('[data-queue-dock] > div').boundingBox(),
          harness.page.locator('[data-testid="todo-panel"]').boundingBox(),
          harness.page.locator('[data-goal-bar] > div').boundingBox(),
        ]),
        ([queue, todo, goal]) => queue !== null && todo !== null && goal !== null
          && todo.y < goal.y && goal.y < queue.y
          && Math.abs(todo.x - goal.x) < 0.05
          && Math.abs(todo.x - queue.x) < 0.05
          && Math.abs(todo.width - goal.width) < 0.05
          && Math.abs(todo.width - queue.width) < 0.05,
      )
      expect(queueBox).not.toBeNull()
      expect(todoBox).not.toBeNull()
      expect(goalBox).not.toBeNull()
      expect(todoBox!.y).toBeLessThan(goalBox!.y)
      expect(goalBox!.y).toBeLessThan(queueBox!.y)
      expect(todoBox!.x).toBeCloseTo(goalBox!.x, 1)
      expect(todoBox!.x).toBeCloseTo(queueBox!.x, 1)
      expect(todoBox!.width).toBeCloseTo(goalBox!.width, 1)
      expect(todoBox!.width).toBeCloseTo(queueBox!.width, 1)
    }
    await aligned()
    await harness.page.setViewportSize({ width: 640, height: 1000 })
    await aligned()
    await harness.page.setViewportSize({ width: 1680, height: 1000 })

    await queueHeader.click()
    const removeButtons = harness.page.getByRole('button', { name: 'Remove queued message' })
    await expect(waitUntil(() => removeButtons.count(), count => count === 2)).resolves.toBe(2)
    await removeButtons.first().click()
    await expect(waitUntil(() => removeButtons.count(), count => count === 1)).resolves.toBe(1)
    await removeButtons.first().click()
    await expect(waitUntil(() => harness.page.locator('[data-queue-dock]').count(), count => count === 0)).resolves.toBe(0)
    await harness.page.getByRole('button', { name: 'Clear goal' }).click()
    await expect(waitUntil(() => harness.page.locator('[data-goal-bar]').count(), count => count === 0)).resolves.toBe(0)
    await harness.page.getByRole('button', { name: 'Stop generating' }).click()
    await waitUntil(
      () => harness.sessions(),
      sessions => sessions.some(session => session.sessionId === sessionId && !session.running),
    )
    expect(turnEndReasons(await sessionEvents(harness, sessionId))).toEqual(['aborted'])
    harness.assertClean()
  } finally {
    await harness.close()
    await rm(overrideDir, { recursive: true, force: true })
  }
}, 120_000)
