import { readdir, readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { afterAll, beforeAll, describe, expect, test } from 'bun:test'
import { acknowledgeReloadConnectionLoss, captureStableAria, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/lifecycle-chrome')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const PROMPT = 'Reply with the single word LIGHTHOUSE and stop.'

let harness: RustWebHarness
let sessionId = ''

async function assertGolden(candidate: RustWebHarness, selector: string, file: string): Promise<void> {
  expect(await captureStableAria(candidate.page, selector)).toBe((await Bun.file(join(SNAPSHOT_DIR, file)).text()).trim())
}

async function fixtureUserPrompts(): Promise<string[]> {
  return (await readFile(FIXTURE, 'utf8'))
    .trim()
    .split('\n')
    .map(line => JSON.parse(line) as { type?: string; data?: { content?: Array<{ type?: string; text?: string }> } })
    .filter(row => row.type === 'user/message')
    .flatMap(row => row.data?.content ?? [])
    .filter(block => block.type === 'text')
    .flatMap(block => block.text === undefined ? [] : [block.text])
}

async function sessionEvents(id: string): Promise<Array<{ type?: string; data?: { reason?: { kind?: string } } }>> {
  const path = join(harness.dataDir, `session-${Buffer.from(id).toString('hex')}.jsonl`)
  return (await readFile(path, 'utf8')).trim().split('\n').slice(1).map(line => JSON.parse(line))
}

describe('lifecycle chrome over RustWebHarness', () => {
  beforeAll(async () => {
    harness = await RustWebHarness.launch({
      name: 'lifecycle-chrome',
      locale: 'en-US',
      replayFixture: FIXTURE,
      env: { TESSIVUM_REPLAY_PACE_MS: '100' },
    })
  }, 120_000)

  afterAll(async () => {
    await harness?.close()
  })

  test('opens the shared slash menu from plus with only Command candidates', async () => {
    const launcher = harness.page.getByRole('button', { name: 'Commands' })
    await launcher.click()
    const menu = harness.page.getByRole('listbox', { name: 'Trigger suggestions' })
    await menu.waitFor({ timeout: 10_000 })
    await assertGolden(harness, '[role="listbox"]', 'command-menu.expected.md')
    const snapshot = await captureStableAria(harness.page, '[role="listbox"]')
    expect(snapshot).toContain('text: Commands')
    expect(snapshot).not.toContain('text: Skills')
    expect(snapshot).not.toContain('text: Subagents')
    const launchedBox = await menu.boundingBox()
    const input = harness.page.locator('textarea').first()
    await input.press('Escape')
    await waitUntil(() => menu.count(), count => count === 0, 10_000)
    await input.fill('/')
    await menu.waitFor({ timeout: 10_000 })
    const typedBox = await menu.boundingBox()
    expect(launchedBox).not.toBeNull()
    expect(typedBox).not.toBeNull()
    expect(Math.abs(launchedBox!.x - typedBox!.x)).toBeLessThan(1)
    expect(Math.abs(launchedBox!.y + launchedBox!.height - typedBox!.y - typedBox!.height)).toBeLessThan(1)
    await input.fill('/cpt')
    await waitUntil(() => menu.getByRole('option').allTextContents(), options => (
      JSON.stringify(options) === JSON.stringify(['compactCompact older conversation history'])
    ), 10_000)
    await assertGolden(harness, '[role="listbox"]', 'command-menu-fuzzy.expected.md')
    await input.fill('')
    await waitUntil(() => menu.count(), count => count === 0, 10_000)
  }, 60_000)

  test('shows active Plan as the warn-state status action', async () => {
    const active = await RustWebHarness.launch({ name: 'lifecycle-plan', locale: 'en-US' })
    try {
      const input = active.page.locator('textarea').first()
      await active.page.getByRole('button', { name: 'Commands' }).click()
      const menu = active.page.getByRole('listbox', { name: 'Trigger suggestions' })
      await menu.waitFor({ timeout: 10_000 })
      await menu.getByRole('option', { name: 'plan Enter or leave plan mode' }).click()
      await waitUntil(() => input.inputValue(), value => value === '/plan ', 10_000)
      await input.press('Enter')
      const planButton = active.page.getByRole('button', { name: 'Plan mode on, press to turn off' })
      await planButton.waitFor({ timeout: 10_000 })
      await waitUntil(() => input.inputValue(), value => value === '', 10_000)
      await assertGolden(active, '[class*="frame"]', 'plan-active.expected.md')
      const planStyle = await planButton.evaluate(element => {
        const probe = document.createElement('span')
        probe.style.color = 'var(--dsw-alias-state-warn-label)'
        probe.style.backgroundColor = 'var(--dsw-alias-state-warn-tertiary)'
        document.body.append(probe)
        const actual = getComputedStyle(element)
        const reference = getComputedStyle(probe)
        const result = {
          color: actual.color,
          backgroundColor: actual.backgroundColor,
          borderRadius: actual.borderRadius,
          fontSize: actual.fontSize,
          referenceColor: reference.color,
          referenceBackgroundColor: reference.backgroundColor,
        }
        probe.remove()
        return result
      })
      expect(planStyle.color).toBe(planStyle.referenceColor)
      expect(planStyle.backgroundColor).toBe(planStyle.referenceBackgroundColor)
      expect(planStyle.borderRadius).toBe('999px')
      expect(planStyle.fontSize).toBe('13px')
      await planButton.click()
      await waitUntil(() => planButton.count(), count => count === 0, 10_000)
      active.assertClean()
    } finally {
      await active.close()
    }
  }, 120_000)

  test('sends the first prompt from the empty-state hero', async () => {
    expect(await fixtureUserPrompts()).toEqual([PROMPT])
    await waitUntil(() => harness.page.getByText('Principle and implementation, in concert.', { exact: false }).count(), count => count === 1, 15_000)
    const input = harness.page.locator('textarea').first()
    await input.waitFor({ timeout: 10_000 })
    await assertGolden(harness, '[class*="frame"]', 'hero.expected.md')
    const settled = harness.whenTurnSettled(180_000)
    const originalViewport = harness.page.viewportSize() ?? { width: 1680, height: 1000 }
    await harness.page.setViewportSize({ width: 480, height: 1000 })
    try {
      await input.fill(PROMPT)
      await input.press('Enter')
      const liveTail = harness.page.locator('[data-variant="think"][data-state="running"] [data-follow-end]')
      await waitUntil(
        () => liveTail.evaluateAll(elements => elements.some(element => (
          element.scrollLeft >= element.scrollWidth - element.clientWidth - 1
        ))),
        value => value,
        10_000,
      )
      sessionId = await settled
    } finally {
      await harness.page.setViewportSize(originalViewport)
    }
  }, 200_000)

  test('materializes a real Workspace and Session over the wire', async () => {
    await waitUntil(
      () => harness.page.locator('[role="treeitem"][aria-expanded]').filter({ hasText: 'workspace' }).count(),
      count => count >= 1,
      15_000,
    )
    await waitUntil(() => harness.page.locator('[role="treeitem"][aria-selected="true"]').count(), count => count === 1, 10_000)
    await waitUntil(() => harness.page.getByText('LIGHTHOUSE', { exact: true }).count(), count => count >= 1, 15_000)
    expect((await harness.sessions()).filter(session => !session.blank).map(session => session.cwd)).toEqual([harness.workspace])
    const turnEnds = (await sessionEvents(sessionId)).filter(event => event.type === 'turn/end')
    expect(turnEnds).toHaveLength(1)
    expect(turnEnds[0]?.data?.reason?.kind).toBe('completed')
  }, 60_000)

  test('recovers the whole surface across a reload from the log alone', async () => {
    const warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await waitUntil(() => harness.page.getByText('LIGHTHOUSE', { exact: true }).count(), count => count >= 1, 15_000)
    await waitUntil(() => harness.page.locator('[role="treeitem"][aria-selected="true"]').count(), count => count === 1, 10_000)
    await assertGolden(harness, '[class*="centerCol"]', 'reloaded.expected.md')
    expect(harness.pageErrors).toEqual([])
  }, 90_000)

  test('cascades the dark theme from the body attribute to painted surfaces', async () => {
    const sample = async (): Promise<{ token: string; sidebarBg: string; bodyBg: string }> => (
      harness.page.evaluate(() => {
        const sidebar = document.querySelector('[class*="sidebar"], [class*="rail"]') ?? document.body
        return {
          token: getComputedStyle(document.body).getPropertyValue('--dsw-alias-bg-base').trim(),
          sidebarBg: getComputedStyle(sidebar).backgroundColor,
          bodyBg: getComputedStyle(document.body).backgroundColor,
        }
      })
    )
    const light = await sample()
    await harness.page.evaluate(() => { document.body.setAttribute('data-ds-dark-theme', '') })
    const dark = await sample()
    expect(dark.token).not.toBe(light.token)
    expect(dark.sidebarBg !== light.sidebarBg || dark.bodyBg !== light.bodyBg).toBe(true)
    await harness.page.evaluate(() => { document.body.removeAttribute('data-ds-dark-theme') })
    expect(await sample()).toEqual(light)
    expect(harness.pageErrors).toEqual([])
  }, 60_000)

  test('keeps the fixture inventory closed', async () => {
    expect(harness.warnings).toEqual([])
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
      'command-menu-fuzzy.expected.md',
      'command-menu.expected.md',
      'hero.expected.md',
      'plan-active.expected.md',
      'reloaded.expected.md',
      'session.jsonl',
    ])
    harness.assertClean()
  })
})
