import { expect, test } from 'bun:test'
import { openSessionByMarker, RustWebHarness, settledRecording, textReplay, waitUntil, withSubagents } from './support'

const PARENT = 'subagent-interrupt-parent'
const CHILD = 'subagent-interrupt-child'
const INITIAL = 'Explain event sourcing in one sentence.'
const FOLLOW_UP = 'Now give the same explanation to a human reader.'
const WAKING = 'And add one concrete example.'

function heldReplay(): string {
  const chunks = [
    { type: 'block-start', index: 0, blockType: 'text' },
    ...Array.from({ length: 200 }, () => ({ type: 'text-delta', index: 0, text: 'x' })),
    { type: 'block-end', index: 0, block: { type: 'text', text: 'interrupted text' } },
    { type: 'finish', reason: { kind: 'stop' } },
  ]
  return JSON.stringify({ sessionId: CHILD, provider: 'recorded', model: 'recorded', requestId: 'held', chunks })
}

function emptyChildRecording(): string {
  return `${JSON.stringify({
    type: 'session', version: 0, id: '{{sessionId}}', createdAt: 1_785_000_000_000, cwd: '{{cwd}}',
    parentSession: PARENT, origin: 'subagent', delegationDepth: 1,
  })}\n`
}

test('subagent.interrupt parks a queued follow-up and a waking prompt preserves FIFO', async () => {
  const harness = await RustWebHarness.launch({
    name: 'subagent-interrupt',
    env: { TESSIVUM_REPLAY_PACE_MS: '50' },
    replayRecording: [textReplay(PARENT, 'PARENT_READY', 'parent'), heldReplay(), textReplay(CHILD, 'FOLLOW_UP_DONE', 'follow-up'), textReplay(CHILD, 'WAKING_DONE', 'waking')].join('\n'),
    beforeStart: async candidate => {
      await candidate.seedSession(PARENT, withSubagents(PARENT, settledRecording('Interrupt parent', INITIAL, 'PARENT_READY'), [
        { childId: CHILD, label: 'event-sourcing researcher', mode: 'continuable' },
      ]))
      await candidate.seedSession(CHILD, emptyChildRecording())
    },
  })
  try {
    await openSessionByMarker(harness, INITIAL, 'PARENT_READY')
    const parentInput = harness.page.locator('textarea:enabled').last()
    await parentInput.fill('Keep this parent available.')
    const parentSettled = harness.whenTurnSettled()
    await parentInput.press('Enter')
    await parentSettled

    const first = await harness.rpc<{ messageId: string }>('subagent.prompt', {
      parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', content: [{ type: 'text', text: INITIAL }],
    })
    expect(first.ok).toBe(true)
    await waitUntil(
      () => harness.rpc<{ events: Array<{ event: { type: string } }> }>('subagent.history', {
        parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', maxMessages: 100,
      }),
      result => result.ok && result.value?.events.some(({ event }) => event.type === 'assistant/chunk') === true,
      10_000,
    )
    const queued = await harness.rpc<{ messageId: string }>('subagent.prompt', {
      parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', content: [{ type: 'text', text: FOLLOW_UP }],
    })
    expect(queued.ok).toBe(true)
    await waitUntil(
      () => harness.rpc<{ events: Array<{ event: { type: string; data: { message?: { content?: Array<{ text?: string }> } } } }> }>('subagent.history', {
        parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', maxMessages: 100,
      }),
      result => result.ok && result.value?.events.some(({ event }) => event.type === 'agent/inbox/enqueued' && event.data.message?.content?.some(block => block.text === FOLLOW_UP)) === true,
      10_000,
    )
    const settled = harness.whenTurnSettled()
    const interrupt = await harness.rpc<{ accepted: boolean }>('subagent.interrupt', {
      parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable',
    })
    expect(interrupt).toMatchObject({ ok: true, value: { accepted: true } })
    expect(await settled).toBe(CHILD)

    const parked = await harness.rpc<{ events: Array<{ event: { type: string; data: { reason?: { kind?: string } } } }> }>('subagent.history', {
      parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', maxMessages: 100,
    })
    if (!parked.ok || parked.value === undefined) throw new Error(JSON.stringify(parked.error))
    expect(parked.value.events.filter(({ event }) => event.type === 'turn/start')).toHaveLength(1)
    expect(parked.value.events.filter(({ event }) => event.type === 'turn/end').map(({ event }) => event.data.reason?.kind)).toEqual(['aborted'])

    const waking = await harness.rpc<{ messageId: string }>('subagent.prompt', {
      parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', content: [{ type: 'text', text: WAKING }],
    })
    expect(waking.ok).toBe(true)
    const history = await waitUntil(
      () => harness.rpc<{ events: Array<{ event: { type: string; data: { content?: Array<{ text?: string }>; message?: { content?: Array<{ text?: string }> }; reason?: { kind?: string }; source?: { kind?: string } } } }> }>('subagent.history', {
        parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', maxMessages: 100,
      }),
      result => result.ok && result.value?.events.some(({ event }) => event.type === 'assistant/message' && event.data.message?.content?.some(block => block.text === 'WAKING_DONE')) === true && result.value.events.filter(({ event }) => event.type === 'turn/end').length === 3,
      30_000,
    )
    expect(history.ok).toBe(true)
    const events = history.value!.events.map(({ event }) => event)
    const userTexts = events.flatMap(event => event.type === 'user/message' && event.data.source?.kind === 'user' ? event.data.content?.flatMap(block => block.text === undefined ? [] : [block.text]) ?? [] : [])
    expect(userTexts).toEqual([INITIAL, FOLLOW_UP, WAKING])
    expect(events.filter(event => event.type === 'turn/end').map(event => event.data.reason?.kind)).toEqual(['aborted', 'completed', 'completed'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
