import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, stableAria, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/plan-review')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const REVIEW_EXPECTED = join(SNAPSHOT_DIR, 'review.expected.md')
const SIDEBAR_EXPECTED = join(SNAPSHOT_DIR, 'sidebar.expected.md')
const APPROVED_EXPECTED = join(SNAPSHOT_DIR, 'approved.expected.md')

const TASK = 'Plan a small change: add a --greeting flag to a CLI. Do not read or write any files. '
  + 'Call exit_plan_mode with a short plan of at most five bullet points. '
  + 'Once the plan is approved, reply with the single word DONE and stop.'
const LINE = `/plan ${TASK}`

type JsonRecord = Record<string, unknown>
type SessionEvent = { type: string; data: JsonRecord }

function record(value: unknown): value is JsonRecord {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function textMessage(data: JsonRecord): string | undefined {
  const content = data.content
  if (!Array.isArray(content) || !record(content[0])) return undefined
  return typeof content[0].text === 'string' ? content[0].text : undefined
}

function sessionEvents(document: string): SessionEvent[] {
  return document.trim().split('\n').slice(1).map(line => {
    const value = JSON.parse(line) as unknown
    if (!record(value) || typeof value.type !== 'string' || !record(value.data)) {
      throw new Error('invalid durable session event')
    }
    return { type: value.type, data: value.data }
  })
}

async function events(harness: RustWebHarness, sessionId: string): Promise<SessionEvent[]> {
  const path = join(harness.dataDir, `session-${Buffer.from(sessionId).toString('hex')}.jsonl`)
  return sessionEvents(await readFile(path, 'utf8'))
}

function userPrompts(events: readonly SessionEvent[]): string[] {
  return events.flatMap(event => {
    const source = event.data.source
    return event.type === 'user/message' && record(source) && source.kind === 'user'
      ? [textMessage(event.data)].filter((text): text is string => text !== undefined)
      : []
  })
}

function planModes(events: readonly SessionEvent[]): boolean[] {
  return events.flatMap(event => event.type === 'plan/mode' && typeof event.data.active === 'boolean'
    ? [event.data.active]
    : [])
}

test('reviews the plan on a decision card and approves through the response wire', async () => {
  expect(userPrompts(sessionEvents(await readFile(FIXTURE, 'utf8')))).toEqual([TASK])
  const harness = await RustWebHarness.launch({ name: 'plan-review', locale: 'en-US', replayFixture: FIXTURE })
  try {
    const input = harness.page.locator('textarea').first()
    await input.waitFor({ timeout: 10_000 })
    await input.fill(LINE)
    await input.press('Enter')

    const card = harness.page.locator('[data-plan-review-key]')
    await card.waitFor({ timeout: 30_000 })
    expect(await harness.page.locator('[data-question-key]').count()).toBe(0)
    expect(await waitUntil(() => card.getByText('Plan review').count(), count => count > 0, 10_000)).toBeGreaterThan(0)

    const selectedRow = harness.page.locator('[role="treeitem"][aria-selected="true"]')
    expect(await waitUntil(() => selectedRow.locator('[data-state="warning"]').count(), count => count === 1, 10_000)).toBe(1)
    expect(await waitUntil(() => selectedRow.getByText('Plan awaiting review', { exact: true }).count(), count => count === 1, 10_000)).toBe(1)
    expect(stableAria(await card.ariaSnapshot())).toBe((await readFile(REVIEW_EXPECTED, 'utf8')).trim())
    expect(stableAria(await selectedRow.ariaSnapshot())).toBe((await readFile(SIDEBAR_EXPECTED, 'utf8')).trim())

    const response = harness.page.waitForResponse(value => value.url().endsWith('/api/respond'), { timeout: 10_000 })
    await card.getByRole('button', { name: 'Approve' }).click()
    expect(await (await response).json()).toEqual({ accepted: true })

    const sessions = await harness.sessions()
    const sessionId = sessions.find(item => !item.blank)?.sessionId
    if (sessionId === undefined) throw new Error('plan review created no nonblank session')
    const durable = await waitUntil(
      () => events(harness, sessionId),
      current => current.some(event => event.type === 'turn/end'),
      120_000,
    )
    expect(planModes(durable)).toEqual([true, false])
    expect(JSON.stringify(durable.filter(event => event.type === 'question/resolved').at(-1))).toContain('Approve')
    expect(JSON.stringify(durable.filter(event => event.type === 'tool/result').at(-1))).toContain('Plan approved')
    expect(durable.filter(event => event.type === 'session/model-selected').map(event => event.data)).toEqual([
      { provider: 'deepseek-official', model: 'deepseek-v4-flash' },
    ])
    const models = await harness.rpc<{ current: { provider: string; model: string }; groups: Array<{ id: string }> }>(
      'session.models',
      { sessionId },
    )
    expect(models).toMatchObject({ ok: true, value: { current: { provider: 'deepseek-official', model: 'deepseek-v4-flash' } } })
    expect(models.value?.groups.some(group => group.id === 'deepseek-official')).toBe(true)
    expect(await waitUntil(() => harness.page.getByText('DONE', { exact: true }).count(), count => count >= 1)).toBeGreaterThanOrEqual(1)
    expect(await card.count()).toBe(0)
    expect(await selectedRow.locator('[data-state="warning"]').count()).toBe(0)
    expect(await waitUntil(() => input.isEnabled(), enabled => enabled, 10_000)).toBe(true)
    await harness.page.getByRole('button', { name: /^Select model, current/ }).waitFor({ timeout: 10_000 })
    expect(stableAria(await harness.page.locator('[class*="centerCol"]').ariaSnapshot()))
      .toBe((await readFile(APPROVED_EXPECTED, 'utf8')).trim())
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 200_000)

test('plan review fixture inventory remains closed', async () => {
  expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
    'session.jsonl', 'review.expected.md', 'sidebar.expected.md', 'approved.expected.md',
  ].sort())
})
