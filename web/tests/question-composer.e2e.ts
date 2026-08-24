import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, stableAria, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/question-composer')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const UI_EXPECTED = join(SNAPSHOT_DIR, 'ui.expected.md')
const SIDEBAR_EXPECTED = join(SNAPSHOT_DIR, 'sidebar.expected.md')
const COMPOSED_EXPECTED = join(SNAPSHOT_DIR, 'composed.expected.md')
const ANSWERED_EXPECTED = join(SNAPSHOT_DIR, 'answered.expected.md')
const PROMPT = 'Use the ask_user_question tool to ask me exactly one multi-select question with id "color", question "Which color do you prefer?", header "Pick one", and two options: label "Blue" with description "A cool recessive hue that reads as calm and trustworthy in long reading sessions and dense dashboards.", and label "Green" with description "A restful mid-spectrum hue with the highest perceived brightness, easiest on the eye over long sessions." Set multi_select to true. After I answer, reply with the single word DONE and stop.'

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

function toolResultText(event: Event): string | undefined {
  const message = isObject(event.data.message) ? event.data.message : undefined
  const content = message === undefined || !Array.isArray(message.content) ? [] : message.content
  return content.flatMap((block) => {
    if (!isObject(block) || block.type !== 'tool-result' || !Array.isArray(block.content)) return []
    return textBlocks(block.content)
  }).at(-1)
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

test('asks through the composer, answers, and completes with the answer logged', async () => {
  const harness = await RustWebHarness.launch({
    name: 'question-composer-web-e2e', locale: 'en-US', replayFixture: FIXTURE,
  })
  try {
    const fixturePrompts = (await readFile(FIXTURE, 'utf8')).trim().split('\n').slice(1).flatMap((line) => {
      const event = parseEvent(line)
      return event.type === 'user/message' && isObject(event.data.source) && event.data.source.kind === 'user'
        ? textBlocks(event.data.content) : []
    })
    expect(fixturePrompts).toEqual([PROMPT])

    const input = harness.page.locator('textarea').first()
    await input.waitFor({ timeout: 10_000 })
    const settled = harness.whenTurnSettled(30_000)
    await input.fill(PROMPT)
    await input.press('Enter')

    const composer = harness.page.locator('[data-question-key]')
    await composer.waitFor({ timeout: 30_000 })
    await expect(waitUntil(() => composer.getByText('Which color do you prefer?').count(), count => count > 0)).resolves.toBeGreaterThan(0)
    const selectedRow = harness.page.locator('[role="treeitem"][aria-selected="true"]')
    await expect(waitUntil(() => selectedRow.locator('[data-state="warning"]').count(), count => count === 1)).resolves.toBe(1)
    await expect(waitUntil(() => selectedRow.getByText('Waiting for answer', { exact: true }).count(), count => count === 1)).resolves.toBe(1)

    expect(`${await captureStableAria(harness, '[data-question-key]')}\n`).toBe(await readFile(UI_EXPECTED, 'utf8'))
    expect(`${await captureStableAria(harness, '[role="treeitem"][aria-selected="true"]')}\n`).toBe(await readFile(SIDEBAR_EXPECTED, 'utf8'))

    const original = harness.page.viewportSize() ?? { width: 1680, height: 1000 }
    for (const height of [520, 440, 380]) {
      await harness.page.setViewportSize({ width: 900, height })
      const squeeze = await composer.evaluate((card) => {
        const rows = [...card.querySelectorAll<HTMLElement>('[role="radio"], [role="checkbox"], [aria-expanded]')]
        const spill = rows.map(row => Math.max(...[...row.children].map((child) => {
          const box = row.getBoundingClientRect()
          const inner = child.getBoundingClientRect()
          return Math.max(box.top - inner.top, inner.bottom - box.bottom)
        })))
        const list = card.querySelector<HTMLElement>('[data-question-scroll]')
        return {
          rows: rows.length,
          spill: Math.max(...spill),
          wrappedRows: rows.filter(row => row.getBoundingClientRect().height > 42).length,
          scrolls: list !== null && list.scrollHeight > list.clientHeight,
        }
      })
      expect(squeeze.rows).toBeGreaterThan(0)
      expect(squeeze.wrappedRows).toBeGreaterThan(0)
      expect(squeeze.scrolls).toBe(true)
      expect(squeeze.spill).toBeLessThan(0.6)
    }
    await harness.page.setViewportSize(original)

    const blue = composer.getByRole('checkbox', { name: 'Blue' })
    await blue.click()
    const custom = composer.getByRole('textbox')
    await custom.fill('Include accessibility notes')
    expect(await blue.getAttribute('aria-checked')).toBe('true')
    expect(await custom.inputValue()).toBe('Include accessibility notes')
    expect(`${await captureStableAria(harness, '[data-question-key]')}\n`).toBe(await readFile(COMPOSED_EXPECTED, 'utf8'))

    const response = harness.page.waitForResponse(value => value.url().endsWith('/api/respond'), { timeout: 10_000 })
    await custom.press('Enter')
    expect(await (await response).json()).toEqual({ accepted: true })
    const sessionId = await settled
    const log = await sessionEvents(harness, sessionId)
    const result = log.findLast(event => event.type === 'tool/result')
    if (result === undefined) throw new Error('question answer produced no tool result')
    expect(JSON.parse(toolResultText(result) ?? '')).toEqual({
      answers: [{ id: 'color', selected: ['Blue'], custom: 'Include accessibility notes' }],
    })
    expect(log.filter(event => event.type === 'question/asked')).toHaveLength(1)
    expect(log.filter(event => event.type === 'question/resolved')).toHaveLength(1)
    await expect(waitUntil(() => harness.page.getByText('DONE', { exact: true }).count(), count => count > 0)).resolves.toBeGreaterThanOrEqual(1)
    expect(await harness.page.locator('[data-question-key]').count()).toBe(0)
    expect(await selectedRow.locator('[data-state="warning"]').count()).toBe(0)
    await expect(waitUntil(() => harness.page.locator('textarea').first().isEnabled(), Boolean)).resolves.toBe(true)
    expect(`${await captureStableAria(harness, '[class*="centerCol"]')}\n`).toBe(await readFile(ANSWERED_EXPECTED, 'utf8'))
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
      'answered.expected.md', 'composed.expected.md', 'session.jsonl', 'sidebar.expected.md', 'ui.expected.md',
    ].sort())
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 200_000)
