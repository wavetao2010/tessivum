import { expect, test } from 'bun:test'
import { RustWebHarness } from './support'

const WORKSPACE_INSTRUCTION = 'SMOKE_REAL_WORKSPACE_CONTEXT must reach the model request.'

const ROUND = 'SMOKE_REAL_ROUND_COMPLETE'
const PTC = 'SMOKE_REAL_PTC_COMPLETE'

interface RequestHeader {
  system?: string
  tools?: { name: string }[]
}

interface HistoryEvent {
  event: { type: string; data: { header?: RequestHeader } & Record<string, unknown> }
}

function replayRecording(marker: string, requiredRequest?: string): string {
  const text = requiredRequest === undefined ? marker : `${marker} {{fromRequest:(${requiredRequest})}}`
  const chunks = [
    { type: 'block-start', index: 0, blockType: 'text' },
    { type: 'text-delta', index: 0, text },
    { type: 'block-end', index: 0, block: { type: 'text', text } },
    { type: 'usage', usage: { inputTokens: 16, outputTokens: 4 } },
    { type: 'finish', reason: { kind: 'stop' } },
  ]
  return [
    { type: 'session', version: 0, id: `smoke-real-${marker}`, createdAt: 0, cwd: '/workspace' },
    ...chunks.map((chunk, seq) => ({ type: 'assistant/chunk', seq, time: 0, data: { turn: 1, step: 1, chunk } })),
  ].map(row => JSON.stringify(row)).join('\n')
}

function responseStream(marker: string): string {
  const message = {
    id: `msg-${marker}`,
    type: 'message',
    role: 'assistant',
    status: 'completed',
    content: [{ type: 'output_text', text: marker, annotations: [] }],
  }
  return [
    { type: 'response.created', response: { id: `resp-${marker}` } },
    { type: 'response.output_item.added', output_index: 0, item: { ...message, content: [] } },
    { type: 'response.output_text.delta', output_index: 0, delta: marker },
    { type: 'response.output_item.done', output_index: 0, item: message },
    { type: 'response.completed', response: { id: `resp-${marker}`, status: 'completed', output: [message], usage: { input_tokens: 1, output_tokens: 1 } } },
  ].map(event => `data: ${JSON.stringify(event)}\n\n`).join('')
}

test('retries a partial live Responses stream before surfacing the completed answer', async () => {
  const marker = 'SMOKE_REAL_PARTIAL_RETRY_COMPLETE'
  const discarded = 'SMOKE_REAL_PARTIAL_DISCARDED'
  let attempts = 0
  const provider = Bun.serve({
    hostname: '127.0.0.1',
    port: 0,
    fetch(request) {
      if (new URL(request.url).pathname !== '/v1/responses') return new Response(null, { status: 404 })
      attempts += 1
      const body = attempts === 1
        ? [
          { type: 'response.created', response: { id: 'partial' } },
          { type: 'response.output_item.added', output_index: 0, item: { id: 'partial', type: 'message', role: 'assistant', status: 'in_progress', content: [] } },
          { type: 'response.output_text.delta', output_index: 0, delta: discarded },
        ].map(event => `data: ${JSON.stringify(event)}\n\n`).join('')
        : responseStream(marker)
      return new Response(body, { headers: { 'content-type': 'text/event-stream' } })
    },
  })
  let harness: RustWebHarness | undefined
  try {
    harness = await RustWebHarness.launch({
      name: 'smoke-real-partial-retry',
      env: {
        OPENAI_API_KEY: 'test-key',
        OPENAI_BASE_URL: `http://127.0.0.1:${provider.port}/v1`,
        OPENAI_MODEL: 'smoke-model',
      },
    })
    const settled = harness.whenTurnSettled()
    await harness.page.locator('textarea:enabled').last().fill('Retry the incomplete live stream.')
    await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
    const sessionId = await settled
    await harness.page.getByText(marker, { exact: true }).waitFor({ timeout: 15_000 })
    const history = await harness.rpc<{ events: HistoryEvent[] }>('session.history', { sessionId, maxMessages: 100 })
    expect(history.ok).toBe(true)
    expect(attempts).toBe(2)
    const retry = history.value?.events.find(({ event }) => event.type === 'llm/retry')
    expect(retry?.event.data).toMatchObject({
      turn: 1, step: 1, retry: 1, maxRetries: 2, failure: { code: 'TRANSPORT' },
    })
    expect(JSON.stringify(history.value?.events)).toContain(discarded)
    harness.assertClean()
  } finally {
    await harness?.close()
    provider.stop(true)
  }
}, 120_000)

function tracks(harness: RustWebHarness): Promise<string[]> {
  return harness.page.locator('[class*="frame"]').evaluate(element =>
    getComputedStyle(element).gridTemplateColumns.split(' '))
}

test('the loopback Web host completes a conversation and retains the live surface through tabs, resize, theme, and reload', async () => {
  const harness = await RustWebHarness.launch({
    name: 'smoke-real-web-e2e',
    replayRecording: replayRecording(ROUND, WORKSPACE_INSTRUCTION),
    viewport: { width: 1680, height: 1000 },
    beforeStart: async candidate => { await Bun.write(`${candidate.workspace}/AGENTS.md`, `${WORKSPACE_INSTRUCTION}\n`) },
  })
  try {
    expect(harness.baseUrl).toMatch(/^http:\/\/127\.0\.0\.1:\d+$/)
    expect(await tracks(harness)).toHaveLength(3)

    const settled = harness.whenTurnSettled()
    await harness.page.locator('textarea:enabled').last().fill('Complete the smoke round.')
    await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
    const sessionId = await settled
    await harness.page.getByText(ROUND, { exact: false }).waitFor({ timeout: 15_000 })
    const contextHistory = await harness.rpc<{ events: HistoryEvent[] }>('session.history', { sessionId, maxMessages: 100 })
    expect(JSON.stringify(contextHistory.value?.events)).toContain(WORKSPACE_INSTRUCTION)
    await harness.page.getByText(WORKSPACE_INSTRUCTION, { exact: false }).waitFor({ timeout: 15_000 })
    await harness.page.getByRole('tab', { name: 'Trajectory', exact: true }).click()
    await harness.page.locator('[data-trajectory-scroll]').waitFor({ timeout: 15_000 })
    await harness.page.getByRole('tab', { name: 'Chat', exact: true }).click()
    await harness.page.getByText(ROUND, { exact: false }).waitFor({ timeout: 15_000 })

    const frame = harness.page.locator('[class*="frame"]').first()
    const before = (await tracks(harness))[0]
    const handle = harness.page.locator('[data-side="sidebar"]').first()
    const box = await handle.boundingBox()
    expect(box).not.toBeNull()
    if (box === null) throw new Error('sidebar resize handle has no bounding box')
    await harness.page.mouse.move(box.x + box.width / 2, box.y + 300)
    await harness.page.mouse.down()
    await harness.page.mouse.move(box.x + 70, box.y + 300, { steps: 6 })
    await harness.page.mouse.up()
    expect((await tracks(harness))[0]).not.toBe(before)

    const colors = await harness.page.evaluate(() => {
      document.body.setAttribute('data-ds-dark-theme', '')
      const dark = getComputedStyle(document.body).backgroundColor
      document.body.removeAttribute('data-ds-dark-theme')
      return { dark, light: getComputedStyle(document.body).backgroundColor }
    })
    expect(colors.dark).not.toBe(colors.light)

    await harness.page.reload({ waitUntil: 'load' })
    await frame.waitFor({ timeout: 30_000 })
    expect(await tracks(harness)).toHaveLength(3)
    expect((await tracks(harness))[0]).toBe(before)
    await harness.page.getByText(ROUND, { exact: false }).waitFor({ timeout: 30_000 })

    const history = await harness.rpc<{ events: HistoryEvent[] }>('session.history', { sessionId, maxMessages: 100 })
    expect(history.ok).toBe(true)
    const header = history.value?.events.find(({ event }) => event.type === 'request/header')?.event.data.header
    expect(header?.tools?.map(tool => tool.name)).toContain('web_search')
    expect(history.value?.events.some(({ event }) => event.type === 'assistant/message')).toBe(true)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)

test('PTC projects the shipped SDK tool boundary instead of the direct native catalog', async () => {
  const harness = await RustWebHarness.launch({
    name: 'smoke-real-ptc-web-e2e',
    toolsMode: 'code',
    replayRecording: replayRecording(PTC),
  })
  try {
    const settled = harness.whenTurnSettled()
    await harness.page.locator('textarea:enabled').last().fill('Complete the PTC smoke round.')
    await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
    const sessionId = await settled
    await harness.page.getByText(PTC, { exact: true }).waitFor({ timeout: 15_000 })

    const history = await harness.rpc<{ events: HistoryEvent[] }>('session.history', { sessionId, maxMessages: 100 })
    expect(history.ok).toBe(true)
    const header = history.value?.events.find(({ event }) => event.type === 'request/header')?.event.data.header
    expect(header?.tools?.map(tool => tool.name)).toEqual(['run_code'])
    expect(header?.system).toContain('run_code')
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
