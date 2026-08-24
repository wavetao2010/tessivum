import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

const WIDTHS = [1680, 1200, 1000, 800, 600]
const CONTROL_STYLE_ID = 'conversation-column-overflow-control'
const WHEEL_DELTA = 300

interface ColumnMetrics {
  bleedRange: number
  columnWidth: number
  glowBleeds: boolean
  overflowX: string
  scrollLeft: number
  scrollsVertically: boolean
  width: number
}

function measureColumn(harness: RustWebHarness, width: number): Promise<ColumnMetrics> {
  return harness.page.evaluate(viewportWidth => {
    const scroller = document.querySelector<HTMLElement>('[data-conversation-scroll]')
    const glow = scroller?.querySelector<SVGElement>('[class*="heroGlow"]')
    if (scroller === null || glow === null) throw new Error('conversation hero is not laid out')
    const box = scroller.getBoundingClientRect()
    const glowBox = glow.getBoundingClientRect()
    return {
      bleedRange: scroller.scrollWidth - scroller.clientWidth,
      columnWidth: scroller.clientWidth,
      glowBleeds: glowBox.right > box.left + scroller.clientWidth + 0.5 || glowBox.left < box.left - 0.5,
      overflowX: getComputedStyle(scroller).overflowX,
      scrollLeft: scroller.scrollLeft,
      scrollsVertically: getComputedStyle(scroller).overflowY === 'auto',
      width: viewportWidth,
    }
  }, width)
}

async function settleAt(harness: RustWebHarness, width: number): Promise<ColumnMetrics> {
  await harness.page.setViewportSize({ width, height: 900 })
  await harness.page.locator('[data-conversation-scroll] [class*="heroGlow"]').waitFor({ timeout: 15_000 })
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
  return measureColumn(harness, width)
}

async function wheelHorizontally(harness: RustWebHarness): Promise<number> {
  const origin = await harness.page.locator('[data-conversation-scroll]').evaluate(scroller => {
    scroller.scrollLeft = 0
    const box = scroller.getBoundingClientRect()
    return { x: box.left + box.width / 2, y: box.top + 60 }
  })
  await harness.page.mouse.move(origin.x, origin.y)
  await harness.page.mouse.wheel(WHEEL_DELTA, 0)
  await harness.page.waitForTimeout(400)
  return harness.page.locator('[data-conversation-scroll]').evaluate(scroller => scroller.scrollLeft)
}

async function horizontalScrollLimit(harness: RustWebHarness): Promise<number> {
  return harness.page.locator('[data-conversation-scroll]').evaluate((scroller, delta) => {
    const behavior = scroller.style.scrollBehavior
    scroller.style.scrollBehavior = 'auto'
    scroller.scrollLeft = delta
    const limit = scroller.scrollLeft
    scroller.scrollLeft = 0
    scroller.style.scrollBehavior = behavior
    return limit
  }, WHEEL_DELTA)
}

test('conversation-column-overflow', async () => {
  const harness = await RustWebHarness.launch({ name: 'conversation-column-overflow-web-e2e', viewport: { width: 1680, height: 900 } })
  try {
    const stops: ColumnMetrics[] = []
    for (const width of WIDTHS) {
      const metrics = await settleAt(harness, width)
      metrics.scrollLeft = await wheelHorizontally(harness)
      stops.push(metrics)
    }
    expect(stops.map(stop => stop.width)).toEqual(WIDTHS)
    expect(stops.filter(stop => stop.glowBleeds).map(stop => stop.width)).toEqual([1200, 1000, 800, 600])
    for (const stop of stops) {
      expect(stop.overflowX, `viewport ${stop.width}`).toBe('hidden')
      expect(stop.scrollsVertically, `viewport ${stop.width}`).toBe(true)
      expect(stop.scrollLeft, `viewport ${stop.width}`).toBe(0)
    }
    for (const stop of stops.filter(stop => stop.glowBleeds)) expect(stop.bleedRange, `viewport ${stop.width}`).toBeGreaterThan(0)

    await harness.page.evaluate(id => {
      const style = document.createElement('style')
      style.id = id
      style.textContent = '[data-conversation-scroll] { overflow-x: auto !important; }'
      document.head.append(style)
    }, CONTROL_STYLE_ID)
    try {
      const control = await settleAt(harness, 600)
      expect(control.overflowX).toBe('auto')
      expect(control.bleedRange).toBeGreaterThan(0)
      const limit = await horizontalScrollLimit(harness)
      expect(limit).toBeGreaterThan(0)
      expect(limit).toBeLessThan(WHEEL_DELTA)
      expect(Math.round(await wheelHorizontally(harness))).toBe(Math.round(limit))
    } finally {
      await harness.page.evaluate(id => document.getElementById(id)?.remove(), CONTROL_STYLE_ID)
    }
    await waitUntil(() => measureColumn(harness, 600), metrics => metrics.overflowX === 'hidden')
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
