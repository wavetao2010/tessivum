import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import {
  acknowledgeReloadConnectionLoss, captureStableAria, RustWebHarness, waitUntil,
} from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/goal-command-presentation')
const UI_EXPECTED = join(SNAPSHOT_DIR, 'ui.expected.md')

type Event = { type: string; data: Record<string, unknown> }

async function events(harness: RustWebHarness, sessionId: string): Promise<Event[]> {
  const history = await harness.rpc<{ events: Array<{ event: Event }> }>('session.history', { sessionId, maxMessages: 1_000 })
  if (!history.ok || history.value === undefined) throw new Error(`session.history failed: ${JSON.stringify(history.error)}`)
  return history.value.events.map(entry => entry.event)
}

let harness: RustWebHarness
let sessionId = ''

test('goal command shows its bare input and result without a model turn', async () => {
  harness = await RustWebHarness.launch({ name: 'goal-command-presentation', locale: 'en-US' })
  try {
    await waitUntil(() => harness.page.getByText('Principle and implementation, in concert.', { exact: false }).count(), count => count === 1, 15_000)
    const input = harness.page.locator('textarea').first()
    await input.fill('/goal')
    await input.press('Enter')
    await waitUntil(() => input.inputValue(), value => value === '/goal ')
    await input.press('Enter')

    const commandInput = harness.page.locator('[data-command-input]')
    await commandInput.waitFor({ timeout: 10_000 })
    await waitUntil(() => commandInput.textContent(), value => value === '/goal')
    expect(await commandInput.getAttribute('role')).toBe('group')
    expect(await commandInput.getAttribute('aria-label')).toBe('Command input')
    expect(await commandInput.getByRole('button').count()).toBe(0)
    const typography = await commandInput.evaluate((element) => {
      const bubble = element.firstElementChild?.firstElementChild
      if (!(bubble instanceof HTMLElement)) throw new Error('command input bubble is missing')
      const rootStyle = getComputedStyle(element)
      const bubbleStyle = getComputedStyle(bubble)
      return {
        fontFamily: bubbleStyle.fontFamily,
        parentFontFamily: rootStyle.fontFamily,
        fontSize: bubbleStyle.fontSize,
        lineHeight: bubbleStyle.lineHeight,
      }
    })
    expect(typography).toMatchObject({ fontSize: '14px', lineHeight: '22px' })
    expect(typography.fontFamily).not.toBe(typography.parentFontFamily)

    const resultRow = harness.page.locator('[data-variant="others"]').filter({ hasText: 'No goal is currently set.' })
    await waitUntil(() => resultRow.count(), count => count === 1, 10_000)
    expect(await resultRow.getByText('goal', { exact: true }).count()).toBe(1)
    await waitUntil(() => harness.page.locator('[data-phase="active"]').count(), count => count === 1, 10_000)
    expect(await harness.page.getByText('Principle and implementation, in concert.', { exact: false }).count()).toBe(0)

    const sessions = await harness.sessions()
    expect(sessions).toHaveLength(1)
    sessionId = sessions[0]!.sessionId
    const log = await events(harness, sessionId)
    const run = log.find(event => event.type === 'command/run')
    expect(run).toMatchObject({
      type: 'command/run',
      data: { name: 'goal', args: ' ', source: { kind: 'user' } },
    })
    expect(log.some(event => event.type === 'command/done')).toBe(true)
    expect(log.some(event => event.type === 'user/message')).toBe(false)
    expect(log.some(event => event.type === 'turn/start')).toBe(false)
    expect(log.some(event => event.type === 'step/start')).toBe(false)
    expect(log.some(event => event.type === 'request/header')).toBe(false)

    expect(await captureStableAria(harness.page, '[class*="centerCol"]')).toBe((await readFile(UI_EXPECTED, 'utf8')).trim())
  } catch (error) {
    await harness.close()
    throw error
  }
}, 60_000)

test('goal command reloads its persisted bubble and result', async () => {
  try {
    const warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    acknowledgeReloadConnectionLoss(harness, warningStart)

    await waitUntil(() => harness.page.locator('[data-command-input]').textContent(), value => value === '/goal', 15_000)
    const resultRow = harness.page.locator('[data-variant="others"]').filter({ hasText: 'No goal is currently set.' })
    await waitUntil(() => resultRow.count(), count => count === 1, 10_000)
    await waitUntil(() => harness.page.locator('[data-phase="active"]').count(), count => count === 1, 10_000)

    const sessions = await harness.sessions()
    expect(sessions).toHaveLength(1)
    const persisted = await events(harness, sessionId)
    expect(persisted.filter(event => event.type === 'command/run' || event.type === 'command/done')
      .map(event => event.type)).toEqual(['command/run', 'command/done'])
    expect(persisted.some(event => event.type === 'user/message')).toBe(false)
    expect(persisted.some(event => event.type === 'turn/start')).toBe(false)
    expect(persisted.some(event => event.type === 'step/start')).toBe(false)
    expect(persisted.some(event => event.type === 'request/header')).toBe(false)
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['ui.expected.md'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 90_000)
