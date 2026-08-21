import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/fresh-round-trip')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const UI_EXPECTED = join(SNAPSHOT_DIR, 'ui.expected.md')
const PROMPT = 'Use the bash tool to run exactly: echo WEB_E2E_OK. Then reply with the single word DONE and stop.'

type Event = { type: string; data: Record<string, unknown> }

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function parseEvent(value: unknown): Event {
  if (!record(value) || typeof value.type !== 'string' || !record(value.data)) throw new Error('durable event is malformed')
  return { type: value.type, data: value.data }
}

async function fixturePrompts(): Promise<string[]> {
  return (await readFile(FIXTURE, 'utf8')).trim().split('\n').flatMap((line) => {
    const event = parseEvent(JSON.parse(line))
    const source = record(event.data.source) ? event.data.source : undefined
    const content = Array.isArray(event.data.content) ? event.data.content : []
    return event.type === 'user/message' && source?.kind === 'user'
      ? content.flatMap(block => record(block) && block.type === 'text' && typeof block.text === 'string' ? [block.text] : [])
      : []
  })
}

async function durableEvents(harness: RustWebHarness, sessionId: string): Promise<Event[]> {
  const history = await harness.rpc<{ events: Array<{ event: unknown }> }>('session.history', { sessionId, maxMessages: 1_000 })
  if (!history.ok || history.value === undefined) throw new Error(`session.history failed: ${JSON.stringify(history.error)}`)
  return history.value.events.map(entry => parseEvent(entry.event))
}

test('replay-round-trip drives native bash, persists the transcript, and reconstructs it after reload', async () => {
  expect(await fixturePrompts()).toEqual([PROMPT])
  const harness = await RustWebHarness.launch({
    name: 'replay-round-trip-web-e2e',
    locale: 'en-US',
    replayFixture: FIXTURE,
  })
  try {
    const input = harness.page.locator('textarea:enabled').first()
    const settled = harness.whenTurnSettled()
    await input.fill(PROMPT)
    await input.press('Enter')
    const sessionId = await settled
    await harness.page.getByText('DONE', { exact: true }).last().waitFor({ timeout: 15_000 })

    const events = await durableEvents(harness, sessionId)
    const bash = events.find(event => event.type === 'tool/call' && event.data.name === 'bash')
    if (bash === undefined) throw new Error('replayed turn did not durably call bash')
    const result = events.find(event => event.type === 'tool/result' && JSON.stringify(event.data).includes('WEB_E2E_OK'))
    if (result === undefined) throw new Error('native bash result was not durable')
    expect(JSON.stringify(result.data)).toContain('WEB_E2E_OK')
    expect(events.filter(event => event.type === 'turn/end').map(event => JSON.stringify(event.data.reason))).toEqual(['{"kind":"completed"}'])
    expect(events.filter(event => event.type === 'assistant/chunk').length).toBeGreaterThan(10)

    await harness.page.reload({ waitUntil: 'load' })
    await harness.page.getByText('DONE', { exact: true }).last().waitFor({ timeout: 15_000 })
    expect(await durableEvents(harness, sessionId)).toEqual(events)
    expect(`${await captureStableAria(harness.page, '[class*="centerCol"]')}\n`).toBe(await readFile(UI_EXPECTED, 'utf8'))

    const think = harness.page.getByRole('button', { name: /^Think/ }).first()
    expect(await think.getAttribute('aria-expanded')).toBe('false')
    await think.click()
    await waitUntil(() => think.getAttribute('aria-expanded'), value => value === 'true', 5_000)
    await think.click()
    await waitUntil(() => think.getAttribute('aria-expanded'), value => value === 'false', 5_000)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)

test('replay-round-trip fixture inventory remains closed', async () => {
  expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
    'session.jsonl', 'system-prompt.expected.md', 'ui.expected.md',
  ].sort())
})
