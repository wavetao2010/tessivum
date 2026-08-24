import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, openSessionByMarker, RustWebHarness, settledRecording, textReplay, waitUntil, withSubagents } from './support'

const PARENT = 'subagent-interrupt-ui-parent'
const CHILD = 'subagent-interrupt-ui-child'
const LABEL = 'event-sourcing researcher'
const INITIAL = 'Explain event sourcing in one sentence.'
const REARM = 'Keep working until I stop you again.'
const FOLLOW_UP = 'Now give the same explanation to a human reader.'
const WAKING = 'And add one concrete example.'
const FOLLOW_UP_DONE = 'parked follow-up answer'
const WAKING_DONE = 'waking answer'
const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/subagent-interrupt')
const OFFLINE_COMPOSER_EXPECTED = join(SNAPSHOT_DIR, 'offline-composer.expected.md')

type ChildEvent = {
  type: string
  data: {
    content?: Array<{ text?: string }>
    message?: { content?: Array<{ text?: string }> }
    reason?: { kind?: string }
    source?: { kind?: string }
  }
}
function heldReplay(requestId: string): string {
  return JSON.stringify({
    sessionId: CHILD, provider: 'recorded', model: 'recorded', requestId, chunks: [
      { type: 'block-start', index: 0, blockType: 'text' },
      { type: 'text-delta', index: 0, text: 'partial' },
      ...Array.from({ length: 2_000 }, () => ({ type: 'text-delta', index: 0, text: '' })),
      { type: 'finish', reason: { kind: 'stop' } },
    ],
  })
}

function emptyChildRecording(): string {
  return `${JSON.stringify({
    type: 'session', version: 0, id: '{{sessionId}}', createdAt: 1_785_000_000_000, cwd: '{{cwd}}',
    parentSession: PARENT, origin: 'subagent', delegationDepth: 1, agentPreset: 'standard',
  })}\n`
}

function childHistory(harness: RustWebHarness) {
  return harness.rpc<{ events: Array<{ event: ChildEvent }> }>('subagent.history', {
    parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', maxMessages: 100,
  })
}

test('the child composer stops through subagent.interrupt, parks work, and resumes FIFO', async () => {
  const harness = await RustWebHarness.launch({
    name: 'subagent-interrupt-ui',
    locale: 'en-US',
    env: { TESSIVUM_REPLAY_PACE_MS: '50' },
    replayRecording: [
      textReplay(PARENT, 'PARENT_READY'),
      heldReplay('held-first'),
      heldReplay('held-second'),
      textReplay(CHILD, FOLLOW_UP_DONE, 'follow-up'),
      textReplay(CHILD, WAKING_DONE, 'waking'),
    ].join('\n'),
    beforeStart: async candidate => {
      await candidate.seedSession(PARENT, withSubagents(PARENT, settledRecording('Ask a research subagent to', 'Ask a research subagent to explain event sourcing.', 'PARENT_READY'), [
        { childId: CHILD, label: LABEL, mode: 'continuable' },
      ]))
      await candidate.seedSession(CHILD, emptyChildRecording())
    },
  })
  try {
    const calls: string[] = []
    harness.page.on('request', request => calls.push(new URL(request.url()).pathname))
    await openSessionByMarker(harness, 'Ask a research subagent to explain event sourcing.', 'PARENT_READY')
    const parentInput = harness.page.locator('textarea:enabled').last()
    await parentInput.fill('Keep the parent online.')
    const parentSettled = harness.whenTurnSettled()
    await parentInput.press('Enter')
    await parentSettled

    const first = await harness.rpc('subagent.prompt', {
      parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', content: [{ type: 'text', text: INITIAL }],
    })
    expect(first.ok).toBe(true)
    await waitUntil(() => childHistory(harness), result => result.ok && result.value?.events.some(({ event }) => event.type === 'assistant/chunk') === true, 10_000)

    const catalogPattern = '**/api/subagent.list'
    await harness.page.route(catalogPattern, async route => {
      const response = await route.fetch()
      const body = await response.json() as { result: { ok: boolean; value?: { parentAvailable: boolean } } }
      if (body.result.ok && body.result.value !== undefined) body.result.value.parentAvailable = false
      await route.fulfill({ response, json: body })
    })
    try {
      await harness.page.reload({ waitUntil: 'load' })
      await harness.page.getByRole('button', { name: /1 subagent/ }).click()
      await harness.page.getByRole('treeitem', { name: new RegExp(LABEL) }).click()
      const offlineInput = harness.page.getByRole('textbox', { name: 'Parent session offline; sending is unavailable but you can still stop the run' })
      await offlineInput.waitFor()
      await harness.page.getByText(INITIAL, { exact: false }).waitFor()
      await harness.page.getByText('partial', { exact: true }).waitFor()
      expect(await offlineInput.isDisabled()).toBe(true)
      const offlineSend = harness.page.getByRole('button', { name: 'Send message' })
      expect(await offlineSend.isDisabled()).toBe(true)
      const offlineStop = harness.page.getByRole('button', { name: 'Stop generating' })
      expect(await offlineStop.isEnabled()).toBe(true)
      expect(`${await captureStableAria(harness.page, '[class*="centerCol"]')}\n`).toBe(await readFile(OFFLINE_COMPOSER_EXPECTED, 'utf8'))
      const interrupted = harness.page.waitForResponse(item => new URL(item.url()).pathname === '/api/subagent.interrupt')
      await offlineStop.click()
      expect((await (await interrupted).json() as { result: { ok: boolean; value?: { accepted: boolean } } }).result).toMatchObject({ ok: true, value: { accepted: true } })
      expect(calls).not.toContain('/api/session.cancel')
    } finally {
      await harness.page.unroute(catalogPattern)
    }

    const firstParked = await waitUntil(() => childHistory(harness), result => result.ok && result.value?.events.some(({ event }) => event.type === 'turn/end' && event.data.reason?.kind === 'aborted') === true, 30_000)
    expect(firstParked.ok).toBe(true)
    const rearmed = await harness.rpc('subagent.prompt', {
      parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', content: [{ type: 'text', text: REARM }],
    })
    expect(rearmed.ok).toBe(true)
    await waitUntil(() => childHistory(harness), result => result.ok && result.value?.events.filter(({ event }) => event.type === 'turn/start').length === 2 && result.value.events.filter(({ event }) => event.type === 'assistant/chunk').length >= 3, 30_000)

    await harness.page.reload({ waitUntil: 'load' })
    await harness.page.getByRole('treeitem', { name: /Ask a research subagent to/ }).click()
    await harness.page.getByRole('button', { name: /1 subagent/ }).click()
    await harness.page.getByRole('treeitem', { name: new RegExp(LABEL) }).click()
    const input = harness.page.getByRole('textbox', { name: 'Message the agent' })
    await input.waitFor()
    expect(await input.isDisabled()).toBe(false)

    const prompt = harness.page.waitForResponse(item => new URL(item.url()).pathname === '/api/subagent.prompt')
    await input.fill(FOLLOW_UP)
    await harness.page.getByRole('button', { name: 'Send message' }).click()
    expect((await (await prompt).json() as { result: { ok: boolean } }).result).toMatchObject({ ok: true })

    const onlineStop = harness.page.getByRole('button', { name: 'Stop generating' })
    expect(await onlineStop.isEnabled()).toBe(true)
    const interrupted = harness.page.waitForResponse(item => new URL(item.url()).pathname === '/api/subagent.interrupt')
    await onlineStop.click()
    expect((await (await interrupted).json() as { result: { ok: boolean; value?: { accepted: boolean } } }).result).toMatchObject({ ok: true, value: { accepted: true } })
    expect(calls).not.toContain('/api/session.cancel')

    const parked = await waitUntil(() => childHistory(harness), result => result.ok && result.value?.events.filter(({ event }) => event.type === 'turn/end' && event.data.reason?.kind === 'aborted').length === 2, 30_000)
    if (!parked.ok || parked.value === undefined) throw new Error(JSON.stringify(parked.error))
    expect(parked.value.events.filter(({ event }) => event.type === 'turn/start')).toHaveLength(2)
    expect(parked.value.events.filter(({ event }) => event.type === 'turn/end').map(({ event }) => event.data.reason?.kind)).toEqual(['aborted', 'aborted'])
    await harness.page.getByRole('button', { name: 'Send message' }).waitFor()

    await input.fill(WAKING)
    await input.press('Enter')
    const complete = await waitUntil(() => childHistory(harness), result => result.ok && result.value?.events.some(({ event }) => event.type === 'assistant/message' && event.data.message?.content?.some(block => block.text === WAKING_DONE)) === true && result.value.events.filter(({ event }) => event.type === 'turn/end').length === 4, 60_000)
    if (!complete.ok || complete.value === undefined) throw new Error(JSON.stringify(complete.error))
    const events = complete.value.events.map(({ event }) => event)
    const userTexts = events.flatMap(event => event.type === 'user/message' && event.data.source?.kind === 'user'
      ? event.data.content?.flatMap(block => block.text === undefined ? [] : [block.text]) ?? []
      : [])
    expect(userTexts).toEqual([INITIAL, REARM, FOLLOW_UP, WAKING])
    expect(events.filter(event => event.type === 'turn/end').map(event => event.data.reason?.kind)).toEqual(['aborted', 'aborted', 'completed', 'completed'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)

test('subagent interrupt UI fixture inventory remains closed', async () => {
  expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['offline-composer.expected.md'])
})
