import { readFile } from 'node:fs/promises'
import type { Page } from 'playwright-core'
import { expect, test } from 'bun:test'
import { fixture, materializeRecording, RustWebHarness, waitUntil } from './support'

const SEED_COUNT = 24
const NO_THUMB = 'rgba(0, 0, 0, 0)'

interface ListMetrics {
  gutter: string
  width: string
  track: string
  standardWidth: string
  standardColor: string
  hoverRules: string[]
  token: string
  hoverToken: string
  overflows: boolean
  band: number
  scrollbarEdgeOffset: number
  rowEdgeInset: number
  clientRight: number
  borderRight: number
  timeRight: number
  timeCoveredBy: number
}

function measureList(page: Page): Promise<ListMetrics> {
  return page.evaluate(() => {
    const list = document.querySelector<HTMLElement>('[role="tree"][aria-label="Sessions"]')
    if (list === null) throw new Error('sidebar session list not in the DOM')
    const time = list.querySelector<HTMLElement>('[class*="time"]')
    if (time === null) throw new Error('no row relative-time element in the sidebar list')
    const row = list.querySelector<HTMLElement>('[role="treeitem"]')
    if (row === null) throw new Error('no row in the sidebar list')
    const resolve = (name: string): string => {
      const probe = document.createElement('span')
      probe.style.color = `var(${name})`
      list.append(probe)
      const value = getComputedStyle(probe).color
      probe.remove()
      return value
    }
    const hoverRules = [...document.styleSheets]
      .flatMap(sheet => {
        try { return [...sheet.cssRules] } catch { return [] }
      })
      .filter((rule): rule is CSSStyleRule => rule instanceof CSSStyleRule)
      .filter(rule => rule.selectorText === '::-webkit-scrollbar-thumb:hover')
      .map(rule => rule.style.getPropertyValue('background'))
    const style = getComputedStyle(list)
    const width = getComputedStyle(list, '::-webkit-scrollbar').width
    const barWidth = width === 'auto' ? 15 : Number.parseFloat(width)
    const listRect = list.getBoundingClientRect()
    const sidebarEdge = list.parentElement?.getBoundingClientRect().right
    if (sidebarEdge === undefined) throw new Error('sidebar session list has no layout parent')
    const timeRight = time.getBoundingClientRect().right
    return {
      gutter: style.scrollbarGutter,
      width,
      track: getComputedStyle(list, '::-webkit-scrollbar-track').backgroundColor,
      standardWidth: style.scrollbarWidth,
      standardColor: style.scrollbarColor,
      hoverRules,
      token: resolve('--dsh-scrollbar-thumb'),
      hoverToken: resolve('--dsh-scrollbar-thumb-hover'),
      overflows: list.scrollHeight > list.clientHeight,
      band: listRect.width - list.clientWidth,
      scrollbarEdgeOffset: sidebarEdge - listRect.right,
      rowEdgeInset: sidebarEdge - row.getBoundingClientRect().right,
      clientRight: listRect.left + list.clientWidth,
      borderRight: listRect.right,
      timeRight,
      timeCoveredBy: Math.max(0, timeRight - (listRect.right - barWidth)),
    }
  })
}

function measureRowInset(page: Page): Promise<Pick<ListMetrics, 'overflows' | 'rowEdgeInset'>> {
  return page.evaluate(() => {
    const list = document.querySelector<HTMLElement>('[role="tree"][aria-label="Sessions"]')
    const row = list?.querySelector<HTMLElement>('[role="treeitem"]')
    const sidebarEdge = list?.parentElement?.getBoundingClientRect().right
    if (list === null || list === undefined || row === null || row === undefined || sidebarEdge === undefined) {
      throw new Error('sidebar session list geometry is incomplete')
    }
    return { overflows: list.scrollHeight > list.clientHeight, rowEdgeInset: sidebarEdge - row.getBoundingClientRect().right }
  })
}

function resolveThumb(page: Page): Promise<string> {
  return page.evaluate(() => {
    const list = document.querySelector<HTMLElement>('[role="tree"][aria-label="Sessions"]')
    if (list === null) throw new Error('sidebar session list not in the DOM')
    const probe = document.createElement('span')
    probe.style.color = 'var(--dsh-scrollbar-thumb)'
    list.append(probe)
    const value = getComputedStyle(probe).color
    probe.remove()
    return value
  })
}

async function pointAt(page: Page, where: 'list' | 'away'): Promise<void> {
  const box = await page.locator('[role="tree"][aria-label="Sessions"]').boundingBox()
  const viewport = page.viewportSize()
  if (box === null || viewport === null) throw new Error('sidebar session list has no layout box')
  await page.mouse.move(where === 'list' ? box.x + box.width / 2 : viewport.width - 5, box.y + box.height / 2)
}

async function expandSeededSessions(harness: RustWebHarness): Promise<void> {
  const group = harness.page.getByRole('treeitem', { name: /^Ungrouped/ })
  const rows = harness.page.locator('[role="tree"][aria-label="Sessions"] [role="treeitem"]')
  await group.waitFor({ timeout: 15_000 })
  const showMore = harness.page.getByRole('button', { name: /Show \d+ more sessions/ })
  await waitUntil(async () => {
    if (await group.getAttribute('aria-expanded') !== 'true') await group.click()
    if (await rows.count() <= SEED_COUNT / 2 && await showMore.count() !== 0) await showMore.click()
    return rows.count()
  }, count => count > SEED_COUNT / 2, 30_000)
}

interface PaletteMetrics { hovered: ListMetrics; quietThumb: string }

async function measurePalette(page: Page): Promise<PaletteMetrics> {
  await pointAt(page, 'away')
  await waitUntil(() => resolveThumb(page), thumb => thumb === NO_THUMB, 10_000)
  const quietThumb = await resolveThumb(page)
  await pointAt(page, 'list')
  await waitUntil(() => resolveThumb(page), thumb => thumb !== NO_THUMB, 10_000)
  return { hovered: await measureList(page), quietThumb }
}

function renderGeometry(light: PaletteMetrics, dark: PaletteMetrics): string {
  const render = (name: string, palette: PaletteMetrics): string[] => {
    const metrics = palette.hovered
    return [
      `## ${name}`,
      '',
      `- --dsh-scrollbar-thumb, pointer outside the sidebar: ${palette.quietThumb}`,
      `- scrollbar-gutter: ${metrics.gutter}`,
      `- ::-webkit-scrollbar width: ${metrics.width}`,
      `- ::-webkit-scrollbar-track background: ${metrics.track}`,
      `- scrollbar-width: ${metrics.standardWidth}`,
      `- scrollbar-color: ${metrics.standardColor}`,
      `- ::-webkit-scrollbar-thumb:hover declarations: ${metrics.hoverRules.join(' | ')}`,
      `- --dsh-scrollbar-thumb, pointer over the list: ${metrics.token}`,
      `- --dsh-scrollbar-thumb-hover, pointer over the list: ${metrics.hoverToken}`,
      `- list overflows: ${String(metrics.overflows)}`,
      `- reserved band: ${String(metrics.band)}px`,
      `- scrollbar inset from the sidebar edge: ${String(metrics.scrollbarEdgeOffset)}px`,
      `- row background inset from the sidebar edge: ${String(metrics.rowEdgeInset)}px`,
      `- relative time covered by the bar: ${String(metrics.timeCoveredBy)}px`,
      `- relative time ends inside the content area: ${String(metrics.timeRight <= metrics.clientRight)}`,
      `- content area ends before the border box: ${String(metrics.clientRight < metrics.borderRight)}`,
      '',
    ]
  }
  return ['# Sidebar session list scrollbar', '', ...render('Light palette', light), ...render('Dark palette', dark)].join('\n').trimEnd()
}

test('sidebar session list reserves its gutter and renders the themed thumb', async () => {
  const seed = await readFile(await fixture('seeded-history', 'seed.jsonl'), 'utf8')
  const harness = await RustWebHarness.launch({
    name: 'sidebar-scrollbar-web-e2e',
    locale: 'en-US',
    viewport: { width: 1680, height: 800 },
    beforeStart: async candidate => {
      for (let index = 0; index < SEED_COUNT; index += 1) {
        await candidate.seedSession(`sidebar-scrollbar-web-e2e-${String(index).padStart(2, '0')}`, materializeRecording(seed))
      }
    },
  })
  try {
    await expandSeededSessions(harness)
    await pointAt(harness.page, 'list')

    await waitUntil(async () => (await measureList(harness.page)).overflows, Boolean, 10_000)
    const metrics = await measureList(harness.page)
    expect(metrics.gutter).toBe('stable')
    expect(metrics.band).toBeGreaterThan(0)
    expect(metrics.scrollbarEdgeOffset).toBe(2)
    expect(metrics.rowEdgeInset).toBe(12)
    expect(metrics.timeCoveredBy).toBe(0)
    expect(metrics.timeRight).toBeLessThanOrEqual(metrics.clientRight)
    expect(metrics.clientRight).toBeLessThan(metrics.borderRight)

    const revealed = await resolveThumb(harness.page)
    expect(revealed).not.toBe(NO_THUMB)
    await pointAt(harness.page, 'away')
    expect(await resolveThumb(harness.page)).toBe(revealed)
    await waitUntil(() => resolveThumb(harness.page), thumb => thumb === NO_THUMB, 10_000)
    const quiet = await measureList(harness.page)
    expect(quiet.gutter).toBe('stable')
    expect(quiet.band).toBeGreaterThan(0)
    expect(quiet.timeCoveredBy).toBe(0)
    await harness.page.locator('[role="tree"][aria-label="Sessions"]').evaluate(list => { list.scrollTop += 200 })
    await Bun.sleep(500)
    expect(await resolveThumb(harness.page)).toBe(NO_THUMB)
    await pointAt(harness.page, 'list')
    await waitUntil(() => resolveThumb(harness.page), thumb => thumb === revealed, 10_000)

    expect(await measureRowInset(harness.page)).toEqual({ overflows: true, rowEdgeInset: 12 })
    const group = harness.page.getByRole('treeitem', { name: /^Ungrouped/ })
    await group.click()
    try {
      await waitUntil(async () => !(await measureRowInset(harness.page)).overflows, Boolean, 10_000)
      expect(await measureRowInset(harness.page)).toEqual({ overflows: false, rowEdgeInset: 12 })
    } finally {
      await expandSeededSessions(harness)
    }

    const light = await measureList(harness.page)
    expect(light.standardWidth).toBe('auto')
    expect(light.standardColor).toBe('auto')
    expect(light.width).toBe('8px')
    expect(light.track).toBe('rgba(0, 0, 0, 0)')
    expect(light.hoverRules).toEqual(['var(--dsh-scrollbar-thumb-hover)'])
    expect(light.token).toMatch(/^rgba?\(/)
    expect(light.hoverToken).not.toBe(light.token)
    await harness.page.evaluate(() => { document.body.setAttribute('data-ds-dark-theme', '') })
    const dark = await measureList(harness.page)
    expect(dark.token).not.toBe(light.token)
    expect(dark.hoverToken).not.toBe(dark.token)
    expect(dark.hoverToken).not.toBe(light.hoverToken)
    await harness.page.evaluate(() => { document.body.removeAttribute('data-ds-dark-theme') })
    const restored = await measureList(harness.page)
    expect(restored.token).toBe(light.token)
    expect(restored.hoverToken).toBe(light.hoverToken)

    const lightPalette = await measurePalette(harness.page)
    await harness.page.evaluate(() => { document.body.setAttribute('data-ds-dark-theme', '') })
    const darkPalette = await measurePalette(harness.page)
    await harness.page.evaluate(() => { document.body.removeAttribute('data-ds-dark-theme') })
    expect(renderGeometry(lightPalette, darkPalette)).toBe(
      (await readFile(await fixture('sidebar-scrollbar', 'geometry.expected.md'), 'utf8')).trim(),
    )
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)
