import { existsSync } from 'node:fs'
import { mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/turn-tail-actions')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const RUNNING_EXPECTED = join(SNAPSHOT_DIR, 'running.expected.md')
const SETTLED_EXPECTED = join(SNAPSHOT_DIR, 'settled.expected.md')
const PROMPT = 'Begin your reply with the plain sentence "Reading the workspace now." as text, and in that same message call the bash tool with the command "echo alpha". After the tool result, reply with the single word DONE and stop.'
const NARRATION = 'Reading the workspace now.'

type Event = { type: string; data: Record<string, unknown> }

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function parseEvent(line: string): Event {
  const value: unknown = JSON.parse(line)
  if (!record(value) || typeof value.type !== 'string' || !record(value.data)) throw new Error('durable event is malformed')
  return { type: value.type, data: value.data }
}

async function fixtureUserPrompts(): Promise<string[]> {
  return (await readFile(FIXTURE, 'utf8')).trim().split('\n').slice(1).flatMap((line) => {
    const event = parseEvent(line)
    const source = record(event.data.source) ? event.data.source : undefined
    const content = Array.isArray(event.data.content) ? event.data.content : []
    return event.type === 'user/message' && source?.kind === 'user'
      ? content.flatMap(block => record(block) && block.type === 'text' && typeof block.text === 'string' ? [block.text] : [])
      : []
  })
}

async function sessionEvents(harness: RustWebHarness, sessionId: string): Promise<Event[]> {
  const path = join(harness.dataDir, `session-${Buffer.from(sessionId).toString('hex')}.jsonl`)
  return (await readFile(path, 'utf8')).trim().split('\n').slice(1).map(parseEvent)
}

test('turn-tail-actions withholds assistant actions until the parked turn ends', async () => {
  expect(await fixtureUserPrompts()).toEqual([PROMPT])
  const sidecar = await mkdtemp(join(tmpdir(), 'tessivum-turn-tail-actions-'))
  const marker = join(sidecar, '.hang-ready')
  const override = join(sidecar, 'replay.override.json')
  await writeFile(override, JSON.stringify({ patches: [{ at: 1, entry: { kind: 'hang', readyFile: marker } }] }))
  let harness: RustWebHarness | undefined
  try {
    harness = await RustWebHarness.launch({
      name: 'turn-tail-actions-web-e2e',
      locale: 'en-US',
      replayFixture: FIXTURE,
      replayOverride: override,
    })
    const input = harness.page.locator('textarea').first()
    const settled = harness.whenTurnSettled(120_000)
    await input.fill(PROMPT)
    await input.press('Enter')

    await waitUntil(async () => existsSync(marker), Boolean, 20_000)
    await harness.page.getByText(NARRATION, { exact: true }).waitFor({ timeout: 10_000 })
    expect(await harness.page.getByText(NARRATION, { exact: true }).count()).toBe(1)
    const deepDiving = harness.page.getByRole('status').filter({ hasText: 'Deep diving...' })
    await deepDiving.waitFor({ timeout: 10_000 })
    expect(await deepDiving.isVisible()).toBe(true)
    const copies = harness.page.getByRole('button', { name: 'Copy' })
    await waitUntil(() => copies.count(), count => count === 1, 10_000)
    expect(await harness.page.getByRole('button', { name: 'Branch into a new conversation' }).count()).toBe(0)
    await copies.first().focus()
    expect(`${await captureStableAria(harness.page, '[class*="centerCol"]')}\n`).toBe(await readFile(RUNNING_EXPECTED, 'utf8'))

    await harness.page.getByRole('button', { name: 'Stop generating' }).click()
    const sessionId = await settled
    const events = await sessionEvents(harness, sessionId)
    expect(events.filter(event => event.type === 'turn/end').map(event => (record(event.data.reason) ? event.data.reason.kind : undefined))).toEqual(['aborted'])
    await waitUntil(() => copies.count(), count => count === 2, 10_000)
    await waitUntil(() => harness.page.locator('[data-streaming="true"]').count(), count => count === 0, 10_000)
    await copies.last().focus()
    expect(`${await captureStableAria(harness.page, '[class*="centerCol"]')}\n`).toBe(await readFile(SETTLED_EXPECTED, 'utf8'))
    harness.assertClean()
  } finally {
    await harness?.close()
    await rm(sidecar, { recursive: true, force: true })
  }
}, 120_000)

test('turn-tail-actions fixture inventory remains closed', async () => {
  expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
    'running.expected.md', 'session.jsonl', 'settled.expected.md',
  ].sort())
})
