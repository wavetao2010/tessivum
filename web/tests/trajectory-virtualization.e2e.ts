import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, longChatFixture, openSessionByMarker, RustWebHarness, UPSTREAM_TESTS, waitUntil } from './support'

const SESSION_ID = 'trajectory-virtualization-e2e'
const FIXTURE = longChatFixture({ markerPrefix: 'TRAJECTORY_VIRTUAL', title: 'TRAJECTORY_VIRTUAL long ledger' })
const LOAD_MORE_EXPECTED = join(UPSTREAM_TESTS, 'snapshots/trajectory-virtualization/load-more.expected.md')
const TOLERANCE = 2
const MAX_MOUNTED_ROWS = 160
const STREAM_MARKER = 'TRAJECTORY_VIRTUAL_STREAM_FINISHED'

function streamingReplay(): string {
  const chunks = [
    { type: 'block-start', index: 0, blockType: 'text' },
    ...Array.from({ length: 80 }, (_, index) => ({ type: 'text-delta', index: 0, text: `stream fragment ${String(index + 1).padStart(2, '0')} ` })),
    { type: 'text-delta', index: 0, text: STREAM_MARKER },
    { type: 'block-end', index: 0, block: { type: 'text', text: `${Array.from({ length: 80 }, (_, index) => `stream fragment ${String(index + 1).padStart(2, '0')} `).join('')}${STREAM_MARKER}` } },
    { type: 'finish', reason: { kind: 'stop' } },
  ]
  return [
    { type: 'session', version: 0, id: 'trajectory-virtualization-replay', createdAt: 0, cwd: '/workspace' },
    ...chunks.map((chunk, seq) => ({ type: 'assistant/chunk', seq, time: 0, data: { turn: 1, step: 1, chunk } })),
  ].map(row => JSON.stringify(row)).join('\n')
}

async function openTrajectory(harness: RustWebHarness): Promise<void> {
  await harness.page.getByRole('tab', { name: 'Trajectory', exact: true }).click()
  await harness.page.locator('[data-trajectory-scroll] table[data-scroll-ready="true"]').waitFor({ timeout: 30_000 })
}

function logicalRows(harness: RustWebHarness): Promise<number> {
  return harness.page.locator('[data-trajectory-scroll] table').getAttribute('aria-rowcount').then(raw => {
    if (raw === null || !/^\d+$/.test(raw)) throw new Error(`invalid trajectory aria-rowcount ${JSON.stringify(raw)}`)
    return Number(raw)
  })
}

function mountedRows(harness: RustWebHarness): Promise<number> {
  return harness.page.locator('[data-trajectory-scroll] tr[data-trajectory-row-key]').count()
}

async function geometry(harness: RustWebHarness): Promise<{ clientHeight: number; scrollHeight: number; scrollTop: number }> {
  return harness.page.locator('[data-trajectory-scroll]').evaluate(host => ({
    clientHeight: host.clientHeight,
    scrollHeight: host.scrollHeight,
    scrollTop: host.scrollTop,
  }))
}

async function nextPaint(harness: RustWebHarness): Promise<void> {
  await harness.page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))))
}

async function scrollToRatio(harness: RustWebHarness, ratio: number): Promise<void> {
  await harness.page.locator('[data-trajectory-scroll]').evaluate((host, value) => {
    host.scrollTop = Math.round((host.scrollHeight - host.clientHeight) * value)
    host.dispatchEvent(new Event('scroll'))
  }, ratio)
  await nextPaint(harness)
}

function firstVisibleRow(harness: RustWebHarness): Promise<{ key: string; top: number }> {
  return harness.page.locator('[data-trajectory-scroll]').evaluate(host => {
    const box = host.getBoundingClientRect()
    const row = [...host.querySelectorAll<HTMLElement>('tr[data-trajectory-row-key]')].find(candidate => {
      const rect = candidate.getBoundingClientRect()
      return candidate.dataset.requestOnly !== 'true' && rect.bottom > box.top && rect.top < box.bottom
    })
    const key = row?.dataset.trajectoryRowKey
    if (row === undefined || key === undefined) throw new Error('no visible semantic trajectory row')
    return { key, top: row.getBoundingClientRect().top - box.top }
  })
}

function rowTop(harness: RustWebHarness, key: string): Promise<number | null> {
  return harness.page.locator('[data-trajectory-scroll]').evaluate((host, rowKey) => {
    const row = [...host.querySelectorAll<HTMLElement>('tr[data-trajectory-row-key]')].find(candidate => candidate.dataset.trajectoryRowKey === rowKey)
    return row === undefined ? null : row.getBoundingClientRect().top - host.getBoundingClientRect().top
  }, key)
}

async function loadToFirstTurn(harness: RustWebHarness): Promise<void> {
  for (let attempt = 0; attempt < 12; attempt += 1) {
    await scrollToRatio(harness, 0)
    if (await harness.page.getByText(FIXTURE.markers.user(1), { exact: false }).count() > 0) return
    const before = await logicalRows(harness)
    const anchor = await firstVisibleRow(harness)
    await waitUntil(async () => ({
      marker: await harness.page.getByText(FIXTURE.markers.user(1), { exact: false }).count() > 0,
      rows: await logicalRows(harness),
    }), value => value.marker || value.rows > before, 30_000)
    await nextPaint(harness)
    await waitUntil(async () => {
      const top = await rowTop(harness, anchor.key)
      return top !== null && Math.abs(top - anchor.top) <= TOLERANCE
    }, Boolean)
  }
  throw new Error('trajectory did not reach the first turn after twelve older-page requests')
}

test('trajectory-virtualization', async () => {
  const harness = await RustWebHarness.launch({
    name: 'trajectory-virtualization-web-e2e', viewport: { width: 1680, height: 900 }, replayRecording: streamingReplay(),
    beforeStart: candidate => candidate.seedSession(SESSION_ID, FIXTURE.log),
  })
  try {
    await openSessionByMarker(harness, FIXTURE.markers.user(1), FIXTURE.markers.assistant(FIXTURE.turns))
    let held = false
    let releaseHistory = (): void => {}
    let finishHeldRequest = (): void => {}
    const gate = new Promise<void>(resolve => { releaseHistory = resolve })
    const heldFinished = new Promise<void>(resolve => { finishHeldRequest = resolve })
    await harness.page.route('**/api/session.history', async route => {
      const request = route.request().postDataJSON() as { method?: string; payload?: { beforeSeq?: number } }
      if (!held && request.method === 'session.history' && request.payload?.beforeSeq !== undefined) {
        held = true
        await gate
        try {
          await route.continue()
        } finally {
          finishHeldRequest()
        }
      } else {
        await route.continue()
      }
    })
    try {
      await openTrajectory(harness)
      const initialRows = await logicalRows(harness)
      expect(initialRows).toBeGreaterThan(0)
      expect(await harness.page.getByText('Initial System Prompt', { exact: true }).count()).toBe(0)
      expect(await mountedRows(harness)).toBeLessThanOrEqual(MAX_MOUNTED_ROWS)
      const loadMore = harness.page.locator('[data-history-load] button')
      await loadMore.waitFor({ timeout: 15_000 })
      expect(await loadMore.textContent()).toBe('Load earlier history')
      expect(`${await captureStableAria(harness.page, '[data-history-load]')}\n`).toBe(await readFile(LOAD_MORE_EXPECTED, 'utf8'))
      await loadMore.evaluate(button => (button as HTMLButtonElement).click())
      await waitUntil(() => Promise.resolve(held), Boolean)
      await waitUntil(async () => ({ disabled: await loadMore.isDisabled(), label: await loadMore.getAttribute('aria-label') }), value => value.disabled && value.label === 'Loading earlier history…')

      await scrollToRatio(harness, 0)
      const anchor = await firstVisibleRow(harness)
      const selected = harness.page.locator(`[data-trajectory-scroll] tr[data-trajectory-row-key=${JSON.stringify(anchor.key)}]`)
      await selected.click()
      await waitUntil(() => selected.getAttribute('aria-selected'), value => value === 'true')
      releaseHistory()
      await waitUntil(async () => (await logicalRows(harness)) > initialRows, Boolean, 60_000)
      await nextPaint(harness)
      await waitUntil(async () => {
        const top = await rowTop(harness, anchor.key)
        return top !== null && Math.abs(top - anchor.top) <= TOLERANCE
      }, Boolean)
      expect(await selected.getAttribute('aria-selected')).toBe('true')
      expect(await mountedRows(harness)).toBeLessThanOrEqual(MAX_MOUNTED_ROWS)

      await loadToFirstTurn(harness)
      expect(await harness.page.getByText(FIXTURE.markers.user(1), { exact: false }).count()).toBeGreaterThan(0)
      const fullRows = await logicalRows(harness)
      await scrollToRatio(harness, 0.5)
      const middle = await geometry(harness)
      const maximum = middle.scrollHeight - middle.clientHeight
      expect(middle.scrollTop).toBeGreaterThan(maximum * 0.25)
      expect(middle.scrollTop).toBeLessThan(maximum * 0.75)
      expect(await mountedRows(harness)).toBeLessThanOrEqual(MAX_MOUNTED_ROWS)
      expect(await mountedRows(harness)).toBeLessThan(fullRows)
      await scrollToRatio(harness, 1)
      await waitUntil(async () => {
        const value = await geometry(harness)
        return value.scrollHeight - value.clientHeight - value.scrollTop
      }, distance => distance <= TOLERANCE)
      await waitUntil(() => harness.page.getByText(FIXTURE.markers.assistant(FIXTURE.turns), { exact: false }).count(), count => count > 0)
      expect(await mountedRows(harness)).toBeLessThanOrEqual(MAX_MOUNTED_ROWS)

      const pane = harness.page.locator('[data-trajectory-scroll]')
      await pane.evaluate(host => {
        const measured = window as Window & { __trajectoryScrollCalls?: number }
        measured.__trajectoryScrollCalls = 0
        const original = host.scrollTo.bind(host)
        host.scrollTo = ((...args: [ScrollToOptions?] | [number, number]) => {
          measured.__trajectoryScrollCalls = (measured.__trajectoryScrollCalls ?? 0) + 1
          Reflect.apply(original, host, args)
        }) as typeof host.scrollTo
      })
      const settled = harness.whenTurnSettled()
      const input = harness.page.locator('textarea:enabled').first()
      await input.fill('Stream one deterministic response while Trajectory remains visible.')
      await input.press('Enter')
      await settled
      await harness.page.getByText('stream fragment 01', { exact: false }).waitFor({ timeout: 30_000 })
      await nextPaint(harness)
      expect(await pane.evaluate(() => (window as Window & { __trajectoryScrollCalls?: number }).__trajectoryScrollCalls ?? 0)).toBeLessThanOrEqual(8)
      expect(await mountedRows(harness)).toBeLessThanOrEqual(MAX_MOUNTED_ROWS)
    } finally {
      releaseHistory()
      if (held) await heldFinished
      await harness.page.unroute('**/api/session.history')
    }
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)
