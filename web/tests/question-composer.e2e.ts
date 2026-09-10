import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

const FIXTURE = join(import.meta.dir, 'snapshots/question-composer/session.jsonl')
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
    expect(await composer.getByText('Pick one', { exact: true }).count()).toBe(1)
    expect(await composer.getByRole('checkbox', { name: 'Blue' }).count()).toBe(1)
    expect(await composer.getByRole('checkbox', { name: 'Green' }).count()).toBe(1)
    const selectedRow = harness.page.locator('[role="treeitem"][aria-selected="true"]')
    await expect(waitUntil(() => selectedRow.locator('[data-state="warning"]').count(), count => count === 1)).resolves.toBe(1)
    await expect(waitUntil(() => selectedRow.getByText('Waiting for answer', { exact: true }).count(), count => count === 1)).resolves.toBe(1)

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
    const backToBottom = harness.page.getByRole('button', { name: 'Back to bottom', exact: true })
    if (await backToBottom.count() !== 0) {
      await backToBottom.click()
      await waitUntil(() => backToBottom.count(), count => count === 0)
    }
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 200_000)
