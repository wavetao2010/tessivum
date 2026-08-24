import { expect, test } from 'bun:test'
import { longChatFixture, openSessionByMarker, RustWebHarness, waitUntil } from './support'

const FIXTURE = longChatFixture({ markerPrefix: 'TAB_GEOMETRY', title: 'COMPOSER_TAB_GEOMETRY long session', turns: 24 })
const SESSION_ID = 'composer-tab-geometry-web-e2e'
const WIDE = { width: 1680, height: 1000 }
const NARROW = { width: 800, height: 1000 }
const CONTROL_STYLE_ID = 'composer-tab-geometry-control'

interface TabMetrics {
  band: number
  cardLeft: number
  cardRight: number
  cardWidth: number
  gutter: string
  overflowX: string
  overflowY: string
  scrolls: boolean
}

function measureTab(harness: RustWebHarness): Promise<TabMetrics> {
  return harness.page.evaluate(() => {
    const host = document.querySelector<HTMLElement>('[data-conversation-scroll]')
    const card = host?.querySelector<HTMLElement>('[data-composer-seat] [data-composer-card]')
    if (host === null || card === null) throw new Error('conversation composer geometry is unavailable')
    const style = getComputedStyle(host)
    const hostRect = host.getBoundingClientRect()
    const cardRect = card.getBoundingClientRect()
    return {
      band: hostRect.width - host.clientWidth,
      cardLeft: cardRect.left,
      cardRight: cardRect.right,
      cardWidth: cardRect.width,
      gutter: style.scrollbarGutter,
      overflowX: style.overflowX,
      overflowY: style.overflowY,
      scrolls: host.scrollHeight > host.clientHeight,
    }
  })
}

async function nextPaint(harness: RustWebHarness): Promise<void> {
  await harness.page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))))
}

async function settleViewport(harness: RustWebHarness, viewport: { width: number; height: number }): Promise<void> {
  await harness.page.setViewportSize(viewport)
  await harness.page.locator('[data-conversation-scroll]').evaluate(async host => {
    let previous = host.getBoundingClientRect().width
    let stableFrames = 0
    while (stableFrames < 3) {
      await new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
      const current = host.getBoundingClientRect().width
      stableFrames = Math.abs(current - previous) < 0.01 ? stableFrames + 1 : 0
      previous = current
    }
  })
}

async function showTab(harness: RustWebHarness, tab: 'Chat' | 'Trajectory'): Promise<void> {
  await harness.page.getByRole('tab', { name: tab, exact: true }).click()
  if (tab === 'Chat') {
    await waitUntil(() => harness.page.locator('[data-chat-anchor-key]').evaluateAll(rows => rows.some(row => {
      const rect = row.getBoundingClientRect()
      return rect.width > 0 && rect.height > 0
    })), Boolean, 30_000)
  }
  else await harness.page.locator('[data-trajectory-scroll] table[data-scroll-ready="true"]').waitFor({ timeout: 30_000 })
  await nextPaint(harness)
}

async function compareTabs(harness: RustWebHarness): Promise<{ chat: TabMetrics; trajectory: TabMetrics; leftShift: number; rightShift: number; widthShift: number }> {
  await showTab(harness, 'Chat')
  const chat = await measureTab(harness)
  await showTab(harness, 'Trajectory')
  const trajectory = await measureTab(harness)
  await showTab(harness, 'Chat')
  return {
    chat,
    trajectory,
    leftShift: Math.abs(trajectory.cardLeft - chat.cardLeft),
    rightShift: Math.abs(trajectory.cardRight - chat.cardRight),
    widthShift: Math.abs(trajectory.cardWidth - chat.cardWidth),
  }
}

async function compareWithoutCompensation(harness: RustWebHarness): Promise<{ chat: TabMetrics; trajectory: TabMetrics; leftShift: number; rightShift: number; widthShift: number }> {
  await harness.page.evaluate(id => {
    const style = document.createElement('style')
    style.id = id
    style.textContent = '[data-conversation-scroll]:has([data-conversation-composer-overlay]) > [data-composer-seat] { right: 0 !important; }'
    document.head.append(style)
  }, CONTROL_STYLE_ID)
  try {
    return await compareTabs(harness)
  } finally {
    await harness.page.evaluate(id => document.getElementById(id)?.remove(), CONTROL_STYLE_ID)
  }
}

test('composer-tab-geometry', async () => {
  const harness = await RustWebHarness.launch({
    name: 'composer-tab-geometry-web-e2e', viewport: WIDE,
    beforeStart: candidate => candidate.seedSession(SESSION_ID, FIXTURE.log),
  })
  try {
    await openSessionByMarker(harness, FIXTURE.markers.user(1), FIXTURE.markers.assistant(FIXTURE.turns))
    await settleViewport(harness, WIDE)
    await waitUntil(() => measureTab(harness), metrics => metrics.scrolls)
    const wide = await compareTabs(harness)
    expect(wide.chat.band).toBeGreaterThan(0)
    expect(wide.chat.gutter).toBe('stable')
    expect(wide.chat.overflowX).toBe('hidden')
    expect(wide.chat.overflowY).toBe('auto')
    expect(wide.trajectory.gutter).toBe('auto')
    expect(wide.trajectory.band).toBe(0)
    expect(wide.trajectory.overflowX).toBe('hidden')
    expect(wide.trajectory.overflowY).toBe('auto')
    expect(wide.trajectory.scrolls).toBe(false)
    expect(wide.leftShift).toBe(0)
    expect(wide.rightShift).toBe(0)
    expect(wide.widthShift).toBe(0)

    const capped = wide.chat.cardWidth
    await settleViewport(harness, NARROW)
    const narrow = await compareTabs(harness)
    expect(narrow.chat.cardWidth).toBeLessThan(capped)
    expect(narrow.leftShift).toBe(0)
    expect(narrow.rightShift).toBe(0)
    expect(narrow.widthShift).toBe(0)

    await settleViewport(harness, WIDE)
    const control = await compareWithoutCompensation(harness)
    expect(control.chat.band).toBeGreaterThan(0)
    expect(control.trajectory.band).toBe(0)
    expect(control.leftShift).toBe(control.chat.band / 2)
    expect(control.rightShift).toBe(control.chat.band / 2)
    expect((await compareTabs(harness)).leftShift).toBe(0)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
