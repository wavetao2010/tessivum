import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/steering')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const SETTLED_EXPECTED = join(SNAPSHOT_DIR, 'settled.expected.md')
const STEER_ALL_DIR = join(import.meta.dir, 'snapshots/steer-all')
const STEER_ALL_OVERRIDE = join(STEER_ALL_DIR, 'replay.override.json')
const STEER_ALL_SETTLED = join(STEER_ALL_DIR, 'settled.expected.md')
const PROMPT = 'Use the ask_user_question tool to ask me exactly one question with id "checkpoint", question "Ready to continue?", header "Checkpoint", and options labeled "Yes" and "No". After I answer, reply with one short sentence acknowledging my answer and stop.'
const STEER = 'Interjection: include the word BANANA in your final reply.'
const STEER_ONE = 'Interjection: include the word BANANA in your final reply.'
const STEER_TWO = 'Interjection: include the word ORANGE in your final reply.'
const PACE = { TESSIVUM_REPLAY_PACE_MS: '100' }

type Event = { type: string; data: Record<string, unknown> }

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function parseEvent(line: string): Event {
  const value: unknown = JSON.parse(line)
  if (!record(value) || typeof value.type !== 'string' || !record(value.data)) throw new Error('durable event is malformed')
  return { type: value.type, data: value.data }
}

async function durableEvents(harness: RustWebHarness, sessionId: string): Promise<Event[]> {
  const path = join(harness.dataDir, `session-${Buffer.from(sessionId).toString('hex')}.jsonl`)
  return (await readFile(path, 'utf8')).trim().split('\n').slice(1).map(parseEvent)
}

function claimedMessages(events: readonly Event[], text: string): Event[] {
  return events.filter(event => event.type === 'user/message' && JSON.stringify(event.data.content).includes(text))
}

function assistantContains(events: readonly Event[], text: string): boolean {
  return events.some(event => event.type === 'assistant/message' && JSON.stringify(event.data).includes(text))
}

async function fixturePrompts(): Promise<string[]> {
  return (await readFile(FIXTURE, 'utf8')).trim().split('\n').slice(1).flatMap((line) => {
    const event = parseEvent(line)
    const source = record(event.data.source) ? event.data.source : undefined
    const content = Array.isArray(event.data.content) ? event.data.content : []
    return event.type === 'user/message' && source?.kind === 'user'
      ? content.flatMap(block => record(block) && block.type === 'text' && typeof block.text === 'string' ? [block.text] : [])
      : []
  })
}

async function answerYes(harness: RustWebHarness): Promise<void> {
  const composer = harness.page.locator('[data-question-key]')
  await composer.waitFor({ timeout: 30_000 })
  const yes = composer.getByRole('radio', { name: 'Yes' })
  await yes.click()
  await yes.press('Enter')
}

async function launchSteering(name: string, replayOverride?: string): Promise<RustWebHarness> {
  return RustWebHarness.launch({
    name,
    locale: 'en-US',
    replayFixture: FIXTURE,
    replayOverride,
    env: PACE,
  })
}

test('steering moves one queued occurrence into the live turn and persists the interruption', async () => {
  expect(await fixturePrompts()).toEqual([PROMPT, STEER])
  const harness = await launchSteering('steering-web-e2e')
  try {
    const input = harness.page.locator('textarea').first()
    await input.fill(PROMPT)
    await input.press('Enter')
    await input.fill(STEER)
    await input.press('Enter')

    const queued = harness.page.getByRole('listitem').filter({ hasText: STEER })
    const steer = queued.getByRole('button', { name: 'Steer queued message' })
    await steer.waitFor({ timeout: 10_000 })
    await waitUntil(() => steer.isEnabled(), Boolean, 10_000)
    await steer.click()
    expect((await harness.sessions()).find(item => !item.blank)?.running).toBe(true)
    const pending = harness.page.locator('[data-pending-steering]').filter({ hasText: STEER })
    await harness.page.locator('[data-question-key]').waitFor({ timeout: 30_000 })

    await answerYes(harness)
    const sessionId = (await harness.sessions()).find(item => !item.blank)?.sessionId
    if (sessionId === undefined) throw new Error('steering created no nonblank session')
    const events = await waitUntil(
      () => durableEvents(harness, sessionId),
      current => current.some(event => event.type === 'turn/end'),
      60_000,
    )
    expect(claimedMessages(events, STEER)).toHaveLength(1)
    expect(assistantContains(events, 'BANANA')).toBe(true)
    expect(events.filter(event => event.type === 'turn/end').map(event => JSON.stringify(event.data.reason))).toEqual(['{"kind":"completed"}'])
    await waitUntil(() => harness.page.getByText('BANANA', { exact: false }).count(), count => count >= 2, 15_000)
    await waitUntil(() => harness.page.getByText(STEER, { exact: true }).count(), count => count === 1, 15_000)
    expect(await pending.count()).toBe(0)
    expect(await harness.page.locator('[data-question-key]').count()).toBe(0)
    await waitUntil(
      async () => (await harness.sessions()).find(item => !item.blank)?.running,
      running => running === false,
      15_000,
    )
    await harness.page.getByRole('button', { name: 'Branch into a new conversation' }).waitFor({ timeout: 15_000 })
    expect(`${await captureStableAria(harness.page, '[class*="centerCol"]')}\n`).toBe(await readFile(SETTLED_EXPECTED, 'utf8'))
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)

test('steering Cmd+Enter sends directly to the live turn without creating a queue row', async () => {
  const harness = await launchSteering('steering-composer-shortcut-web-e2e')
  try {
    const input = harness.page.locator('textarea').first()
    const settled = harness.whenTurnSettled(60_000)
    await input.fill(PROMPT)
    await input.press('Enter')
    await harness.page.getByRole('button', { name: 'Stop generating' }).waitFor({ timeout: 10_000 })
    await input.fill(STEER)
    await input.press('Meta+Enter')
    await waitUntil(() => input.inputValue(), value => value === '', 5_000)
    expect(await harness.page.locator('[data-queue-dock]').count()).toBe(0)

    const pending = harness.page.locator('[data-pending-steering]').filter({ hasText: STEER })
    await pending.waitFor({ timeout: 10_000 })
    await answerYes(harness)
    const events = await durableEvents(harness, await settled)
    expect(claimedMessages(events, STEER)).toHaveLength(1)
    await waitUntil(() => harness.page.getByText(STEER, { exact: true }).count(), count => count === 1, 15_000)
    await waitUntil(() => harness.page.getByText('BANANA', { exact: false }).count(), count => count >= 2, 15_000)
    expect(await pending.count()).toBe(0)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 90_000)

test('steering swaps the busy shortcut when Enter is configured to steer', async () => {
  const harness = await launchSteering('steering-swapped-shortcut-web-e2e')
  try {
    await harness.page.getByRole('button', { name: 'Settings', exact: true }).click()
    const dialog = harness.page.getByRole('dialog', { name: 'Settings' })
    await dialog.getByRole('button', { name: 'Queue' }).click()
    await harness.page.getByRole('menuitem', { name: 'Steer' }).click()
    await dialog.getByRole('button', { name: 'Steer' }).waitFor({ timeout: 10_000 })
    await harness.page.keyboard.press('Escape')

    const input = harness.page.locator('textarea').first()
    const settled = harness.whenTurnSettled(60_000)
    await input.fill(PROMPT)
    await input.press('Enter')
    await harness.page.getByRole('button', { name: 'Stop generating' }).waitFor({ timeout: 10_000 })
    const queuedText = 'Queued by the complementary Cmd+Enter shortcut.'
    await input.fill(queuedText)
    await input.press('Meta+Enter')
    const queued = harness.page.locator('[data-queue-dock]').getByRole('listitem').filter({ hasText: queuedText })
    await queued.getByText(queuedText, { exact: true }).waitFor({ timeout: 10_000 })
    expect(await harness.page.locator('[data-pending-steering]').filter({ hasText: queuedText }).count()).toBe(0)
    const active = (await harness.sessions()).find(session => session.running)
    if (active === undefined) throw new Error('swapped shortcut has no active session')
    expect(claimedMessages(await durableEvents(harness, active.sessionId), queuedText)).toHaveLength(0)
    await queued.getByRole('button', { name: 'Remove queued message' }).click()
    await answerYes(harness)
    await settled
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 90_000)

test('steering flushes an empty-draft queue in FIFO order through a durable replay', async () => {
  const harness = await launchSteering('steering-flush-web-e2e', STEER_ALL_OVERRIDE)
  try {
    const input = harness.page.locator('textarea').first()
    const settled = harness.whenTurnSettled(60_000)
    await input.fill(PROMPT)
    await input.press('Enter')
    await input.fill(STEER_ONE)
    await input.press('Enter')
    await input.fill(STEER_TWO)
    await input.press('Enter')
    const dock = harness.page.locator('[data-queue-dock]')
    await dock.getByText('2 queued messages').waitFor({ timeout: 10_000 })
    await dock.getByRole('button').click()
    await dock.getByText(STEER_ONE, { exact: true }).waitFor({ timeout: 10_000 })
    await dock.getByText(STEER_TWO, { exact: true }).waitFor({ timeout: 10_000 })
    expect(await harness.page.locator('[data-pending-steering]').count()).toBe(0)

    await input.press('Meta+Enter')
    await waitUntil(() => harness.page.locator('[data-queue-dock]').count(), count => count === 0, 10_000)
    await harness.page.locator('[data-question-key]').waitFor({ timeout: 30_000 })

    await answerYes(harness)
    const events = await durableEvents(harness, await settled)
    const first = claimedMessages(events, STEER_ONE)
    const second = claimedMessages(events, STEER_TWO)
    expect(first).toHaveLength(1)
    expect(second).toHaveLength(1)
    const [firstEvent] = first
    const [secondEvent] = second
    if (firstEvent === undefined || secondEvent === undefined) throw new Error('steering messages were not durable')
    expect(events.indexOf(firstEvent)).toBeLessThan(events.indexOf(secondEvent))
    expect(assistantContains(events, 'BANANA')).toBe(true)
    expect(assistantContains(events, 'ORANGE')).toBe(true)
    await waitUntil(() => harness.page.getByText(STEER_ONE, { exact: true }).count(), count => count === 1, 15_000)
    await waitUntil(() => harness.page.getByText(STEER_TWO, { exact: true }).count(), count => count === 1, 15_000)
    expect(await harness.page.locator('[data-pending-steering]').count()).toBe(0)
    await waitUntil(
      async () => (await harness.sessions()).find(item => !item.blank)?.running,
      running => running === false,
      15_000,
    )
    await harness.page.getByRole('button', { name: 'Branch into a new conversation' }).waitFor({ timeout: 15_000 })
    expect(`${await captureStableAria(harness.page, '[class*="centerCol"]')}\n`).toBe(await readFile(STEER_ALL_SETTLED, 'utf8'))
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)

test('steering fixture inventories remain closed', async () => {
  expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
    'mid-steer.expected.md', 'session.jsonl', 'settled.expected.md',
  ].sort())
  expect((await readdir(STEER_ALL_DIR)).sort()).toEqual([
    'mid-steer.expected.md', 'replay.override.json', 'settled.expected.md',
  ].sort())
})
