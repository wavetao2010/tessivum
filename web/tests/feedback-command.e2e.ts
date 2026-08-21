import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/feedback-command')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const PROMPT = 'Reply with the single word LIGHTHOUSE and stop.'
const FEEDBACK = 'the diff view is unreadable'

type Event = { type: string; data: Record<string, any> }

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

test('/feedback persists its command lifecycle and native sharing acknowledgement', async () => {
  expect(await fixturePrompts()).toEqual([PROMPT])
  const harness = await RustWebHarness.launch({ name: 'feedback-command', locale: 'en-US', replayFixture: FIXTURE })
  try {
    const input = harness.page.locator('textarea').first()
    const settled = harness.whenTurnSettled(60_000)
    await input.fill(PROMPT)
    await input.press('Enter')
    const sessionId = await settled
    await harness.page.getByText('LIGHTHOUSE', { exact: true }).waitFor({ timeout: 15_000 })

    await input.fill(`/feedback ${FEEDBACK}`)
    await input.press('Enter')
    await harness.page.getByText(/Feedback recorded for session/).waitFor({ timeout: 10_000 })
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
    expect(done?.data.text).toMatch(new RegExp(
      `^Feedback recorded for session ${sessionId}\\nAnonymous user: [0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\\. Session sharing is not configured\\.$`,
      'i',
    ))

    await harness.page.reload({ waitUntil: 'load' })
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    await harness.page.getByText(/Feedback recorded for session/).waitFor({ timeout: 15_000 })
    expect(await harness.page.getByText(/Session sharing is not configured/).count()).toBe(1)
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['ack.expected.md', 'session.jsonl'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
