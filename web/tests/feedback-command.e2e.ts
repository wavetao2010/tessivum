import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { afterAll, beforeAll, expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/feedback-command')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const ACK_EXPECTED = join(SNAPSHOT_DIR, 'ack.expected.md')
const PROMPT = 'Reply with the single word LIGHTHOUSE and stop.'
const FEEDBACK = 'the diff view is unreadable'

type Event = { type: string; seq: number; data: Record<string, unknown> }

async function fixturePrompts(): Promise<string[]> {
  return (await readFile(FIXTURE, 'utf8'))
    .trim()
    .split('\n')
    .map(line => JSON.parse(line) as { type?: string; data?: { content?: Array<{ type?: string; text?: string }> } })
    .filter(row => row.type === 'user/message')
    .flatMap(row => row.data?.content ?? [])
    .flatMap(block => block.type === 'text' && block.text !== undefined ? [block.text] : [])
}

async function events(harness: RustWebHarness, sessionId: string): Promise<Event[]> {
  const history = await harness.rpc<{ events: Array<{ event: Event }> }>('session.history', { sessionId, maxMessages: 1_000 })
  if (!history.ok || history.value === undefined) throw new Error(`session.history failed: ${JSON.stringify(history.error)}`)
  return history.value.events.map(entry => entry.event)
}

let harness: RustWebHarness
let sessionId = ''

beforeAll(async () => {
  expect(await fixturePrompts()).toEqual([PROMPT])
  harness = await RustWebHarness.launch({ name: 'feedback-command', locale: 'en-US', replayFixture: FIXTURE })
}, 120_000)

afterAll(async () => {
  await harness?.close()
})

test('feedback command drives the recorded prompt to a settled turn', async () => {
  const input = harness.page.locator('textarea').first()
  const settled = harness.whenTurnSettled(60_000)
  await input.fill(PROMPT)
  await input.press('Enter')
  sessionId = await settled
  await harness.page.getByText('LIGHTHOUSE', { exact: true }).waitFor({ timeout: 15_000 })
}, 60_000)

test('feedback command records acknowledgement, lifecycle, and reload persistence', async () => {
  const input = harness.page.locator('textarea').first()
  await input.fill(`/feedback ${FEEDBACK}`)
  await input.press('Enter')
  await harness.page.getByText(/Feedback recorded for session/).waitFor({ timeout: 10_000 })
  // Rust has no configured telemetry backend in the Web fixture; retain its
  // honest disclosure rather than pretending the upstream FULL backend exists.
  expect(await harness.page.getByText(/Session sharing is not configured/).count()).toBe(1)

  const log = await waitUntil(() => events(harness, sessionId), candidate => (
    candidate.some(event => event.type === 'command/done' && event.data.commandId !== undefined)
  ))
  const lifecycle = log.filter(event => ['command/run', 'feedback/record', 'command/done'].includes(event.type))
  expect(lifecycle.map(event => event.type)).toEqual(['command/run', 'feedback/record', 'command/done'])
  const [run, record, done] = lifecycle
  expect(run?.data).toMatchObject({ name: 'feedback', source: { kind: 'user' } })
  expect(run?.data.args).toBeUndefined()
  expect(record?.data).toEqual({ text: FEEDBACK })
  expect(done?.data.commandId).toBe(run?.data.commandId)
  expect(done?.data).toMatchObject({ kind: 'success' })
  expect(done?.data.sourceEventSeq).toBe(record?.seq)
  expect(done?.data.text).toMatch(new RegExp(
    `^Feedback recorded for session ${sessionId}\\nAnonymous user: [0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\\. Session sharing is not configured\\.$`,
    'i',
  ))

  const expected = (await readFile(ACK_EXPECTED, 'utf8'))
    .replaceAll('Session sharing is enabled.', 'Session sharing is not configured.')
    .trim()
  const actual = (await captureStableAria(harness.page, '[class*="centerCol"]'))
    .replaceAll(sessionId, '{{sessionId}}')
    .replace(/Anonymous user: .*?(?=\. Session sharing)/g, 'Anonymous user: {{uuid}}')
  expect(actual).toBe(expected)

  await harness.page.reload({ waitUntil: 'load' })
  await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
  await harness.page.getByText(/Feedback recorded for session/).waitFor({ timeout: 15_000 })
  expect(await harness.page.getByText(/Session sharing is not configured/).count()).toBe(1)
  const reloadedLifecycle = (await events(harness, sessionId))
    .filter(event => ['command/run', 'feedback/record', 'command/done'].includes(event.type))
  expect(reloadedLifecycle.map(event => event.type)).toEqual(['command/run', 'feedback/record', 'command/done'])
  const [reloadedRun, reloadedRecord, reloadedDone] = reloadedLifecycle
  expect(reloadedRecord?.data).toEqual({ text: FEEDBACK })
  expect(reloadedDone?.data.commandId).toBe(reloadedRun?.data.commandId)
  expect(reloadedDone?.data.sourceEventSeq).toBe(reloadedRecord?.seq)
}, 60_000)

test('feedback command keeps its fixture inventory closed', async () => {
  expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['ack.expected.md', 'session.jsonl'])
  harness.assertClean()
})
