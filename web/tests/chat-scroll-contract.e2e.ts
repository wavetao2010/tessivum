import { access, writeFile } from 'node:fs/promises'
import { afterAll, beforeAll, expect, test } from 'bun:test'
import { longChatFixture, openSessionByMarker, RustWebHarness, waitUntil } from './support'
import { chromium, type Browser } from 'playwright-core'

const HISTORY_ID = 'chat-scroll-history-e2e'
const TOOL_ID = 'chat-scroll-tool-e2e'
const RESTORE_A_ID = 'chat-scroll-restore-a-e2e'
const RESTORE_B_ID = 'chat-scroll-restore-b-e2e'
const INPUTS_ID = 'chat-scroll-inputs-e2e'
const FLING_ID = 'chat-scroll-fling-e2e'
const TOLERANCE = 2
const REFLOW_TOLERANCE = 32
const TOOL_CALL_ID = 'chat-scroll-live-tool-call'
const TOOL_READY = '.chat-scroll-tool-ready'
const TOOL_RELEASE = '.chat-scroll-tool-release'

const HISTORY = longChatFixture({ markerPrefix: 'HISTORY', title: 'CHAT_SCROLL_HISTORY long paging session' })
const TOOL = longChatFixture({ markerPrefix: 'TOOL', title: 'CHAT_SCROLL_TOOL live tool session' })
const RESTORE_A = longChatFixture({ markerPrefix: 'RESTORE_A', title: 'CHAT_SCROLL_RESTORE_A long session' })
const RESTORE_B = longChatFixture({ markerPrefix: 'RESTORE_B', title: 'CHAT_SCROLL_RESTORE_B comparison session', turns: 32 })

let browser: Browser

beforeAll(async () => {
  browser = await chromium.launch(process.env.TESSIVUM_CHROMIUM === undefined
    ? { channel: 'chrome' }
    : { executablePath: process.env.TESSIVUM_CHROMIUM })
})

afterAll(async () => {
  await browser.close()
})
const INPUTS = longChatFixture({ markerPrefix: 'INPUTS', title: 'CHAT_SCROLL_INPUTS reader input session' })

function textChunks(first: string, done: string, count: number): unknown[] {
  const text = Array.from({ length: count }, (_, index) => index === 0
    ? `${first} `
    : index === count - 1 ? `${done}.` : `stream-chunk-${String(index).padStart(3, '0')} incremental response `.repeat(3))
  const response = text.join('')
  return [
    { type: 'block-start', index: 0, blockType: 'text' },
    ...text.map(value => ({ type: 'text-delta', index: 0, text: value })),
    { type: 'block-end', index: 0, block: { type: 'text', text: response } },
    { type: 'usage', usage: { inputTokens: 512, outputTokens: Math.ceil(response.length / 4) } },
    { type: 'finish', reason: { kind: 'stop' } },
  ]
}

function toolChunks(): unknown[] {
  const command = [`: > ${TOOL_READY}`, `while [ ! -f ${TOOL_RELEASE} ]; do sleep 0.02; done`, 'line=1', `while [ "$line" -le 64 ]; do printf 'CHAT_SCROLL_TOOL_RESULT line %02d\\n' "$line"; line=$((line + 1)); done`].join('; ')
  const argumentsJson = JSON.stringify({ command, description: 'CHAT_SCROLL_TOOL_RESULT' })
  return [
    { type: 'block-start', index: 0, blockType: 'tool-call' },
    { type: 'tool-call-delta', index: 0, id: TOOL_CALL_ID, name: 'bash', argumentsDelta: argumentsJson },
    { type: 'block-end', index: 0, block: { type: 'tool-call', id: TOOL_CALL_ID, name: 'bash', arguments: argumentsJson } },
    { type: 'usage', usage: { inputTokens: 256, outputTokens: 48 } },
    { type: 'finish', reason: { kind: 'tool-calls' } },
  ]
}

function replayRecording(...attempts: unknown[][]): string {
  let seq = 0
  return [
    { type: 'session', version: 0, id: 'chat-scroll-contract-replay', createdAt: 0, cwd: '/workspace' },
    ...attempts.flatMap((chunks, attempt) => chunks.map(chunk => ({ type: 'assistant/chunk', seq: seq++, time: 0, data: { turn: attempt + 1, step: 1, chunk } }))),
  ].map(row => JSON.stringify(row)).join('\n')
}

function scrollGeometry(harness: RustWebHarness): Promise<{ distanceFromBottom: number; scrollTop: number }> {
  return harness.page.locator('[data-conversation-scroll]').evaluate(host => ({
    distanceFromBottom: host.scrollHeight - host.clientHeight - host.scrollTop,
    scrollTop: host.scrollTop,
  }))
}

async function nextPaint(harness: RustWebHarness): Promise<void> {
  await harness.page.evaluate(async () => {
    await Promise.race([
      (async () => {
        await document.fonts.ready
        await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
      })(),
      new Promise<void>(resolve => setTimeout(resolve, 500)),
    ])
  })
}

async function wheelTranscript(harness: RustWebHarness, deltaY: number): Promise<void> {
  const box = await harness.page.locator('[data-conversation-scroll]').boundingBox()
  if (box === null) throw new Error('conversation scrollport has no layout box')
  await harness.page.mouse.move(box.x + box.width / 2, box.y + Math.min(140, box.height / 3))
  await harness.page.mouse.wheel(0, deltaY)
  await nextPaint(harness)
}

async function flingTranscript(harness: RustWebHarness, deltaY: number): Promise<void> {
  await harness.page.locator('[data-conversation-scroll]').evaluate(async (host, delta) => {
    const direction = Math.sign(delta)
    let remaining = Math.abs(delta)
    let velocity = Math.max(120, remaining / 8)
    while (remaining > 0) {
      const step = Math.min(velocity, remaining)
      host.scrollTop += direction * step
      remaining -= step
      velocity = Math.max(48, velocity * 0.9)
      await new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
    }
  }, deltaY)
  await nextPaint(harness)
}

async function wheelToHistoryStart(harness: RustWebHarness): Promise<void> {
  for (let attempt = 0; attempt < 12 && (await scrollGeometry(harness)).scrollTop > 1; attempt += 1) await wheelTranscript(harness, -2_400)
  await waitUntil(() => scrollGeometry(harness), geometry => geometry.scrollTop <= 1)
}
async function wheelUntilMounted(harness: RustWebHarness, selector: string, deltaY: number): Promise<void> {
  for (let attempt = 0; attempt < 16; attempt += 1) {
    if (await harness.page.locator(selector).count() > 0) return
    await wheelTranscript(harness, deltaY)
  }
  throw new Error(`selector did not mount during transcript wheel: ${selector}`)
}

async function wheelUntilVisible(harness: RustWebHarness, selector: string, deltaY: number): Promise<void> {
  const target = harness.page.locator(selector)
  for (let attempt = 0; attempt < 32; attempt += 1) {
    if (await target.count() > 0 && await target.evaluate(row => {
      const host = row.closest<HTMLElement>('[data-conversation-scroll]')
      if (host === null) return false
      const viewport = host.getBoundingClientRect()
      const visibleBottom = host.querySelector<HTMLElement>('[data-composer-seat]')?.getBoundingClientRect().top ?? viewport.bottom
      const rect = row.getBoundingClientRect()
      return rect.bottom > viewport.top && rect.top < visibleBottom
    })) return
    await wheelTranscript(harness, deltaY)
  }
  throw new Error(`selector did not become visible during transcript wheel: ${selector}`)
}


function visibleAnchor(harness: RustWebHarness): Promise<{ key: string; top: number }> {
  return harness.page.locator('[data-conversation-scroll]').evaluate(host => {
    const viewport = host.getBoundingClientRect()
    const composerTop = host.querySelector<HTMLElement>('[data-composer-seat]')?.getBoundingClientRect().top ?? viewport.bottom
    const row = [...host.querySelectorAll<HTMLElement>('[data-chat-anchor-key]')].find(candidate => {
      const rect = candidate.getBoundingClientRect()
      return rect.bottom > viewport.top && rect.top < composerTop
    })
    const key = row?.dataset.chatAnchorKey
    if (row === undefined || key === undefined) throw new Error('conversation scrollport has no visible semantic row')
    return { key, top: row.getBoundingClientRect().top - viewport.top }
  })
}

function anchorTop(harness: RustWebHarness, key: string): Promise<number> {
  return harness.page.locator('[data-chat-anchor-key]').evaluateAll((rows, rowKey) => {
    const row = rows.find(candidate => (candidate as HTMLElement).dataset.chatAnchorKey === rowKey)
    const host = row?.closest<HTMLElement>('[data-conversation-scroll]')
    if (!(row instanceof HTMLElement) || host === null) throw new Error(`chat anchor ${rowKey} is not mounted`)
    return row.getBoundingClientRect().top - host.getBoundingClientRect().top
  }, key)
}

async function expectSameAnchor(
  harness: RustWebHarness,
  anchor: { key: string; top: number },
  tolerance = TOLERANCE,
): Promise<void> {
  const deadline = Date.now() + 15_000
  let difference = Number.POSITIVE_INFINITY
  while (Date.now() < deadline) {
    difference = Math.abs((await anchorTop(harness, anchor.key)) - anchor.top)
    if (difference <= tolerance) return
    await Bun.sleep(50)
  }
  throw new Error(`chat anchor ${anchor.key} moved ${difference}px`)
}

async function expectBottom(harness: RustWebHarness): Promise<void> {
  await waitUntil(async () => Math.abs((await scrollGeometry(harness)).distanceFromBottom) <= 1, Boolean)
}

async function expectMarkerAboveComposer(harness: RustWebHarness, marker: string): Promise<void> {
  const geometry = await harness.page.getByText(marker, { exact: false }).last().evaluate(node => {
    const row = node.closest<HTMLElement>('[data-chat-flow-key], [data-streaming]')
    const composer = node.closest<HTMLElement>('[data-conversation-scroll]')?.querySelector('[data-composer-seat]')
    if (!(row instanceof HTMLElement) || !(composer instanceof HTMLElement)) {
      throw new Error('latest marker or composer geometry is unavailable')
    }
    return { composerTop: composer.getBoundingClientRect().top, rowBottom: row.getBoundingClientRect().bottom }
  })
  expect(geometry.rowBottom).toBeLessThanOrEqual(geometry.composerTop + TOLERANCE)
}

async function loadEarlierWithAnchor(harness: RustWebHarness): Promise<void> {
  await wheelToHistoryStart(harness)
  const button = harness.page.getByRole('button', { name: 'Load earlier', exact: true })
  await button.waitFor({ timeout: 15_000 })
  const anchor = await visibleAnchor(harness)
  const before = await harness.page.locator('[data-chat-flow-key]').count()
  await button.click()
  await waitUntil(async () => (await harness.page.locator('[data-chat-flow-key]').count()) > before, Boolean, 30_000)
  await nextPaint(harness)
  await expectSameAnchor(harness, anchor)
}

async function exists(path: string): Promise<boolean> {
  try {
    await access(path)
    return true
  } catch {
    return false
  }
}

async function eventCount(harness: RustWebHarness, sessionId: string, type: string): Promise<number> {
  const history = await harness.rpc<{ events: Array<{ event: { type: string } }> }>('session.history', { sessionId, maxMessages: 1_000 })
  if (!history.ok || history.value === undefined) throw new Error(`session.history failed: ${JSON.stringify(history.error)}`)
  return history.value.events.filter(entry => entry.event.type === type).length
}

test('chat-scroll-contract preserves a reader anchor across concurrent history and streaming', async () => {
  const first = 'CHAT_SCROLL_LIVE_FIRST'
  const done = 'CHAT_SCROLL_LIVE_DONE'
  const harness = await RustWebHarness.launch({
    browser,
    name: 'chat-scroll-history-web-e2e',
    viewport: { width: 1680, height: 900 },
    replayRecording: replayRecording(textChunks(first, done, 120)),
    env: { TESSIVUM_REPLAY_PACE_MS: '24' },
    beforeStart: candidate => candidate.seedSession(HISTORY_ID, HISTORY.log),
  })
  try {
    await openSessionByMarker(harness, HISTORY.markers.user(1), HISTORY.markers.assistant(HISTORY.turns))
    await expectBottom(harness)
    let held = false
    let release = (): void => {}
    const gate = new Promise<void>(resolve => { release = resolve })
    await harness.page.route('**/api/session.history', async route => {
      const request = route.request().postDataJSON() as { method?: string; payload?: { beforeSeq?: number } }
      if (!held && request.method === 'session.history' && request.payload?.beforeSeq !== undefined) {
        held = true
        await gate
      }
      await route.continue()
    })
    try {
      const settled = harness.whenTurnSettled()
      const input = harness.page.locator('textarea:enabled').last()
      await input.fill('CHAT_SCROLL_LIVE_USER Continue this long conversation while I inspect older history.')
      await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
      await harness.page.getByText(first, { exact: false }).last().waitFor({ timeout: 15_000 })
      await wheelToHistoryStart(harness)
      const before = await harness.page.locator('[data-chat-flow-key]').count()
      await harness.page.getByRole('button', { name: 'Load earlier', exact: true }).click()
      await waitUntil(() => Promise.resolve(held), Boolean)
      await wheelTranscript(harness, 420)
      const anchor = await visibleAnchor(harness)
      const chunksAfterAnchor = await eventCount(harness, HISTORY_ID, 'assistant/chunk')
      await waitUntil(() => eventCount(harness, HISTORY_ID, 'assistant/chunk'), count => count > chunksAfterAnchor + 5, 10_000)
      release()
      await waitUntil(async () => (await harness.page.locator('[data-chat-flow-key]').count()) > before, Boolean, 30_000)
      await nextPaint(harness)
      await expectSameAnchor(harness, anchor)
      await settled
      await waitUntil(() => harness.page.locator('[data-streaming="true"]').count(), count => count === 0)
      await harness.page.getByText(done, { exact: false }).last().waitFor({ timeout: 15_000 })
      await harness.page.unroute('**/api/session.history')
    let additionalPages = 0
    while (additionalPages < 8) {
      await wheelToHistoryStart(harness)
      if (await harness.page.getByRole('button', { name: 'Load earlier', exact: true }).count() === 0) break
      await loadEarlierWithAnchor(harness)
      additionalPages += 1
    }
    expect(additionalPages).toBeGreaterThan(0)
    expect(await harness.page.locator('[data-conversation-scroll]')
      .getByText(HISTORY.markers.user(1), { exact: false }).count()).toBe(1)
    expect(await harness.page.getByRole('button', { name: 'Load earlier', exact: true }).count()).toBe(0)
    } finally {
      release()
      await harness.page.unroute('**/api/session.history')
    }
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)

test('chat-scroll-contract keeps running tool disclosure and bottom ownership across scroll-away', async () => {
  const first = 'CHAT_SCROLL_TOOL_STREAM_FIRST'
  const done = 'CHAT_SCROLL_TOOL_STREAM_DONE'
  const harness = await RustWebHarness.launch({
    browser,
    name: 'chat-scroll-tool-web-e2e',
    viewport: { width: 1680, height: 900 },
    replayRecording: replayRecording(toolChunks(), textChunks(first, done, 84)),
    env: { TESSIVUM_REPLAY_PACE_MS: '24' },
    beforeStart: candidate => candidate.seedSession(TOOL_ID, TOOL.log),
  })
  try {
    await openSessionByMarker(harness, TOOL.markers.user(1), TOOL.markers.assistant(TOOL.turns))
    const ready = `${harness.workspace}/${TOOL_READY}`
    const release = `${harness.workspace}/${TOOL_RELEASE}`
    let released = false
    try {
      const settled = harness.whenTurnSettled()
      await harness.page.locator('textarea:enabled').last().fill('CHAT_SCROLL_TOOL_USER Run the requested diagnostic and then summarize it.')
      await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
      await waitUntil(() => exists(ready), Boolean, 15_000)
      const row = harness.page.locator(`[data-chat-call-id="${TOOL_CALL_ID}"] [data-sample="bash"]`)
      await row.waitFor({ timeout: 15_000 })
      expect(await row.getAttribute('data-state')).toBe('running')
      await expectBottom(harness)
      await wheelTranscript(harness, -1_200)
      await harness.page.getByRole('button', { name: 'Back to bottom', exact: true }).waitFor({ timeout: 10_000 })
      const away = await visibleAnchor(harness)
      const chunksBeforeRelease = await eventCount(harness, TOOL_ID, 'assistant/chunk')
      await writeFile(release, 'release\n')
      released = true
      await waitUntil(() => eventCount(harness, TOOL_ID, 'tool/result'), count => count > 0, 15_000)
      await harness.page.getByText(first, { exact: false }).waitFor({ timeout: 15_000 })
      await waitUntil(() => eventCount(harness, TOOL_ID, 'assistant/chunk'), count => count > chunksBeforeRelease + 5, 15_000)
      await expectSameAnchor(harness, away)
      const chunksAtRepin = await eventCount(harness, TOOL_ID, 'assistant/chunk')
      await harness.page.getByRole('button', { name: 'Back to bottom', exact: true }).click()
      await expectBottom(harness)
      await waitUntil(() => eventCount(harness, TOOL_ID, 'assistant/chunk'), count => count > chunksAtRepin + 5, 15_000)
      await settled
      await waitUntil(() => harness.page.locator('[data-streaming="true"]').count(), count => count === 0)
      await harness.page.getByText(done, { exact: false }).last().waitFor({ timeout: 15_000 })
      await expectBottom(harness)
      await expectMarkerAboveComposer(harness, done)
      const rowSelector = `[data-chat-call-id="${TOOL_CALL_ID}"] [data-sample="bash"]`
      await wheelUntilVisible(harness, rowSelector, -300)
      const callRow = harness.page.locator(`[data-chat-call-id="${TOOL_CALL_ID}"]`)
      const toolAnchor = await row.evaluate(element => {
        const flow = element.closest<HTMLElement>('[data-chat-anchor-key]')
        const host = element.closest<HTMLElement>('[data-conversation-scroll]')
        if (flow?.dataset.chatAnchorKey === undefined || host === null) throw new Error('live tool row has no settled flow identity')
        return { key: flow.dataset.chatAnchorKey, top: flow.getBoundingClientRect().top - host.getBoundingClientRect().top }
      })
      await row.click()
      await waitUntil(() => row.getAttribute('aria-expanded'), value => value === 'true')
      await waitUntil(() => callRow.locator('[data-terminal]').count(), count => count === 1)
      await expectSameAnchor(harness, toolAnchor)
      expect(await harness.page.getByText('CHAT_SCROLL_TOOL_RESULT', { exact: false }).count()).toBeGreaterThan(0)
      await wheelToHistoryStart(harness)
      await harness.page.getByRole('button', { name: 'Back to bottom', exact: true }).click()
      await expectBottom(harness)
      await wheelUntilMounted(harness, rowSelector, -1_100)
      expect(await harness.page.locator(`[data-chat-call-id="${TOOL_CALL_ID}"] [data-sample="bash"]`).getAttribute('aria-expanded')).toBe('true')
      expect(await harness.page.locator(`[data-chat-call-id="${TOOL_CALL_ID}"] [data-terminal]`).count()).toBe(1)
    } finally {
      if (!released) await writeFile(release, 'release\n').catch(() => {})
    }
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)

test('chat-scroll-contract restores session position and keeps composer resize on the column scroll owner', async () => {
  const harness = await RustWebHarness.launch({
    browser,
    name: 'chat-scroll-restore-web-e2e', viewport: { width: 1680, height: 900 },
    beforeStart: async candidate => {
      await candidate.seedSession(RESTORE_A_ID, RESTORE_A.log)
      await candidate.seedSession(RESTORE_B_ID, RESTORE_B.log)
    },
  })
  try {
    await openSessionByMarker(harness, RESTORE_A.markers.user(1), RESTORE_A.markers.assistant(RESTORE_A.turns))
    await loadEarlierWithAnchor(harness)
    await loadEarlierWithAnchor(harness)
    await wheelToHistoryStart(harness)
    await wheelTranscript(harness, 1_300)
    const sessionAnchor = await visibleAnchor(harness)
    await harness.page.getByRole('tab', { name: 'Trajectory', exact: true }).click()
    await harness.page.getByLabel('Trajectory timeline').waitFor({ timeout: 30_000 })
    await harness.page.setViewportSize({ width: 700, height: 900 })
    await harness.page.getByRole('button', { name: 'Open sidebar', exact: true }).click()
    await harness.page.getByRole('tab', { name: 'Chat', exact: true }).click()
    await nextPaint(harness)
    await expectSameAnchor(harness, sessionAnchor, REFLOW_TOLERANCE)
    const reflowedAnchor = { key: sessionAnchor.key, top: await anchorTop(harness, sessionAnchor.key) }

    await openSessionByMarker(harness, RESTORE_B.markers.user(1), RESTORE_B.markers.assistant(RESTORE_B.turns))
    await openSessionByMarker(harness, RESTORE_A.markers.user(1))
    await expectSameAnchor(harness, reflowedAnchor)

    const backToBottom = harness.page.getByRole('button', { name: 'Back to bottom', exact: true })
    await backToBottom.evaluate(button => {
      if (!(button instanceof HTMLElement)) throw new Error('Back-to-bottom control is not an HTML element')
      button.click()
      const trajectory = [...document.querySelectorAll<HTMLElement>('[role="tab"]')]
        .find(tab => tab.textContent?.trim() === 'Trajectory')
      if (!(trajectory instanceof HTMLElement)) throw new Error('Trajectory tab is unavailable during pinned remount')
      trajectory.click()
    })
    await harness.page.getByLabel('Trajectory timeline').waitFor({ timeout: 30_000 })
    await harness.page.getByRole('tab', { name: 'Chat', exact: true }).click()
    await expectBottom(harness)
    await openSessionByMarker(harness, RESTORE_B.markers.user(1), RESTORE_B.markers.assistant(RESTORE_B.turns))
    await openSessionByMarker(harness, RESTORE_A.markers.user(1), RESTORE_A.markers.assistant(RESTORE_A.turns))
    await expectBottom(harness)
    const input = harness.page.locator('textarea:enabled').last()
    const longDraft = Array.from({ length: 18 }, (_, index) => `composer resize line ${String(index + 1).padStart(2, '0')}`).join('\n')
    await input.fill(longDraft)
    await nextPaint(harness)
    await expectBottom(harness)
    await expectMarkerAboveComposer(harness, RESTORE_A.markers.assistant(RESTORE_A.turns))
    await input.fill('short draft')
    await nextPaint(harness)
    await wheelTranscript(harness, -900)
    const resizeAnchor = await visibleAnchor(harness)
    await input.fill(longDraft)
    await nextPaint(harness)
    await expectSameAnchor(harness, resizeAnchor)
    await input.fill('short draft')
    await nextPaint(harness)
    await expectSameAnchor(harness, resizeAnchor)
    const beforeChain = await scrollGeometry(harness)
    await input.hover()
    await harness.page.mouse.wheel(0, -320)
    await waitUntil(() => scrollGeometry(harness), geometry => geometry.scrollTop < beforeChain.scrollTop)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)

test('chat-scroll-contract keyboard PageUp and End own bottom follow', async () => {
  const harness = await RustWebHarness.launch({
    browser,
    name: 'chat-scroll-keyboard-web-e2e', viewport: { width: 1680, height: 900 },
    beforeStart: candidate => candidate.seedSession(INPUTS_ID, INPUTS.log),
  })
  try {
    await openSessionByMarker(harness, INPUTS.markers.user(1), INPUTS.markers.assistant(INPUTS.turns))
    await expectBottom(harness)
    const back = harness.page.getByRole('button', { name: 'Back to bottom', exact: true })
    const lastTool = harness.page.locator(`[data-chat-call-id="chat-scroll-${String(INPUTS.turns).padStart(3, '0')}-1"] [data-sample="bash"]`)
    await lastTool.focus()
    await harness.page.keyboard.press('End')
    await expectBottom(harness)
    expect(await back.count()).toBe(0)
    for (let press = 0; press < 3; press += 1) {
      await harness.page.keyboard.press('PageUp')
      await nextPaint(harness)
    }
    await back.waitFor({ timeout: 10_000 })
    await waitUntil(() => scrollGeometry(harness), geometry => geometry.distanceFromBottom > 100)
    await harness.page.keyboard.press('End')
    await expectBottom(harness)
    expect(await back.count()).toBe(0)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)

test('chat-scroll-contract synthetic fling releases and reacquires streaming bottom follow', async () => {
  const first = 'CHAT_SCROLL_FLING_STREAM_FIRST'
  const done = 'CHAT_SCROLL_FLING_STREAM_DONE'
  const harness = await RustWebHarness.launch({
    browser,
    name: 'chat-scroll-fling-web-e2e',
    viewport: { width: 1680, height: 900 },
    replayRecording: replayRecording(toolChunks(), textChunks(first, done, 240)),
    env: { TESSIVUM_REPLAY_PACE_MS: '24' },
    beforeStart: candidate => candidate.seedSession(FLING_ID, INPUTS.log),
  })
  try {
    await openSessionByMarker(harness, INPUTS.markers.user(1), INPUTS.markers.assistant(INPUTS.turns))
    const ready = `${harness.workspace}/${TOOL_READY}`
    const release = `${harness.workspace}/${TOOL_RELEASE}`
    const back = harness.page.getByRole('button', { name: 'Back to bottom', exact: true })
    const settled = harness.whenTurnSettled()
    let released = false
    try {
      await harness.page.locator('textarea:enabled').last().fill('CHAT_SCROLL_FLING_USER Keep streaming while I fling back through older output.')
      await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
      await waitUntil(() => exists(ready), Boolean, 15_000)
      await expectBottom(harness)
      await flingTranscript(harness, -900)
      await back.waitFor({ timeout: 10_000 })
      const away = await visibleAnchor(harness)
      const chunksBeforeRelease = await eventCount(harness, FLING_ID, 'assistant/chunk')
      await writeFile(release, 'release\n')
      released = true
      await waitUntil(() => eventCount(harness, FLING_ID, 'tool/result'), count => count > 0, 15_000)
      await waitUntil(() => eventCount(harness, FLING_ID, 'assistant/chunk'), count => count > chunksBeforeRelease + 5, 15_000)
      await expectSameAnchor(harness, away)
      for (let attempt = 0; attempt < 8 && (await scrollGeometry(harness)).distanceFromBottom > 1; attempt += 1) await flingTranscript(harness, 1_600)
      await expectBottom(harness)
      expect(await back.count()).toBe(0)
      const chunksAtRepin = await eventCount(harness, FLING_ID, 'assistant/chunk')
      await waitUntil(() => eventCount(harness, FLING_ID, 'assistant/chunk'), count => count > chunksAtRepin + 5, 15_000)
      await expectBottom(harness)
    } finally {
      if (!released) await writeFile(release, 'release\n').catch(() => {})
    }
    await settled
    await waitUntil(() => harness.page.locator('[data-streaming="true"]').count(), count => count === 0)
    await harness.page.getByText(done, { exact: false }).last().waitFor({ timeout: 15_000 })
    await expectBottom(harness)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)
