import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

const TURN_COUNT = 12
const TOOL_TURNS: Record<number, true> = { 4: true, 9: true }

type Event = { type: string; seq: number; data: any }
type TurnSpec = ReturnType<typeof turnSpec>

function turnSpec(index: number) {
  const id = String(index).padStart(3, '0')
  const userMarker = `CONTINUOUS_CHAT_USER_${id}`
  const firstMarker = `CONTINUOUS_CHAT_FIRST_${id}`
  const doneMarker = `CONTINUOUS_CHAT_DONE_${id}`
  const count = index === TURN_COUNT ? 36 : 8
  const deltas = Array.from({ length: count }, (_, chunk) => chunk === 0
    ? `${firstMarker} `
    : chunk === count - 1 ? `${doneMarker}.` : `turn-${id}-chunk-${String(chunk).padStart(2, '0')} keeps semantic ownership stable. `)
  const prompt = index === TURN_COUNT
    ? [
        `${userMarker} Reconcile this accumulated conversation without losing earlier turn ownership.`,
        ...Array.from({ length: 36 }, (_, chunk) => `Context ${String(chunk + 1).padStart(2, '0')}: preserve token-${String(chunk)} and verify ${'payload '.repeat(12).trimEnd()}.`),
        'Return one continuous response and finish with the requested completion marker.',
      ].join('\n')
    : TOOL_TURNS[index] === true
      ? `${userMarker} Run the requested deterministic tool for turn ${index}, then continue.`
      : `${userMarker} Continue this same conversation through turn ${index}.`
  return {
    index, prompt, userMarker, firstMarker, doneMarker, deltas,
    callId: TOOL_TURNS[index] === true ? `continuous-chat-tool-${id}` : undefined,
    toolMarker: TOOL_TURNS[index] === true ? `CONTINUOUS_CHAT_TOOL_RESULT_${id}` : undefined,
  }
}

function textChunks(spec: TurnSpec): unknown[] {
  const text = spec.deltas.join('')
  return [
    { type: 'block-start', index: 0, blockType: 'text' },
    ...spec.deltas.map(value => ({ type: 'text-delta', index: 0, text: value })),
    { type: 'block-end', index: 0, block: { type: 'text', text } },
    { type: 'usage', usage: { inputTokens: Math.ceil(spec.prompt.length / 4), outputTokens: Math.ceil(text.length / 4) } },
    { type: 'finish', reason: { kind: 'stop' } },
  ]
}

function toolChunks(spec: TurnSpec): unknown[] {
  const argumentsJson = JSON.stringify({ command: `printf '${spec.toolMarker}\\n'`, description: spec.toolMarker })
  return [
    { type: 'block-start', index: 0, blockType: 'tool-call' },
    { type: 'tool-call-delta', index: 0, id: spec.callId, name: 'bash', argumentsDelta: argumentsJson },
    { type: 'block-end', index: 0, block: { type: 'tool-call', id: spec.callId, name: 'bash', arguments: argumentsJson } },
    { type: 'usage', usage: { inputTokens: 256, outputTokens: 24 } },
    { type: 'finish', reason: { kind: 'tool-calls' } },
  ]
}

function replayRecording(specs: TurnSpec[]): string {
  const attempts = specs.flatMap(spec => spec.callId === undefined
    ? [{ turn: spec.index, step: 1, chunks: textChunks(spec) }]
    : [
        { turn: spec.index, step: 1, chunks: toolChunks(spec) },
        { turn: spec.index, step: 2, chunks: textChunks(spec) },
      ])
  let seq = 0
  return [
    { type: 'session', version: 0, id: 'continuous-chat-replay', createdAt: 0, cwd: '/workspace' },
    ...attempts.flatMap(attempt => attempt.chunks.map(chunk => ({
      type: 'assistant/chunk', seq: seq++, time: 0, data: { turn: attempt.turn, step: attempt.step, chunk },
    }))),
  ].map(row => JSON.stringify(row)).join('\n')
}

function textBlocks(content: any[]): string {
  return content.filter(block => block.type === 'text').map(block => block.text).join('')
}

function contextKey(kind: string, id: string): string {
  return `${kind.length}:${kind}${id}`
}

async function sessionEvents(harness: RustWebHarness, sessionId: string): Promise<Event[]> {
  const result = await harness.rpc<{ events: Array<{ event: Event }> }>('session.history', { sessionId, maxMessages: 1_000 })
  if (!result.ok || result.value === undefined) throw new Error(`session.history failed: ${JSON.stringify(result.error)}`)
  return result.value.events.map(entry => entry.event)
}

test('chat-continuous-conversation keeps twelve composer turns in one durable session', async () => {
  const specs = Array.from({ length: TURN_COUNT }, (_, index) => turnSpec(index + 1))
  const harness = await RustWebHarness.launch({
    name: 'chat-continuous-conversation-web-e2e',
    locale: 'en-US',
    replayRecording: replayRecording(specs),
    viewport: { width: 1680, height: 900 },
  })
  try {
    const composer = harness.page.locator('textarea:enabled').last()
    await composer.waitFor({ timeout: 15_000 })
    let sessionId: string | undefined

    for (const spec of specs) {
      expect(await composer.inputValue()).toBe('')
      expect(await composer.isEnabled()).toBe(true)
      const eventStart = sessionId === undefined ? 0 : (await sessionEvents(harness, sessionId)).length
      await composer.fill(spec.prompt)
      expect(await composer.inputValue()).toBe(spec.prompt)
      const settled = harness.whenTurnSettled()
      await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
      const listed = await waitUntil(
        () => harness.sessions(),
        items => items.some(item => !item.blank && (sessionId === undefined || item.sessionId === sessionId)),
      )
      const currentId = listed.find(item => !item.blank && (sessionId === undefined || item.sessionId === sessionId))!.sessionId
      await harness.page.getByText(spec.userMarker, { exact: false }).last().waitFor({ timeout: 15_000 })
      const liveEvents = await waitUntil(
        () => sessionEvents(harness, currentId),
        events => events.slice(eventStart).some(event => event.type === 'user/message'
          && event.data.source?.kind === 'user'
          && JSON.stringify(event.data.content).includes(spec.userMarker)),
      )
      const echoedUser = liveEvents.slice(eventStart).find(event => event.type === 'user/message'
        && event.data.source?.kind === 'user'
        && JSON.stringify(event.data.content).includes(spec.userMarker))!
      await harness.page.getByText(spec.firstMarker, { exact: false }).last().waitFor({ timeout: 15_000 })
      const current = await settled
      if (sessionId === undefined) sessionId = current
      else expect(current).toBe(sessionId)
      await waitUntil(() => harness.page.locator('[data-streaming="true"]').count(), count => count === 0)
      await harness.page.getByText(spec.doneMarker, { exact: false }).last().waitFor({ timeout: 15_000 })
      await waitUntil(() => composer.inputValue(), value => value === '')
      await waitUntil(() => composer.isEnabled(), Boolean)
      const turnEvents = await sessionEvents(harness, current)
      const user = turnEvents.find(event => event.seq === echoedUser.seq)
      expect(user?.seq).toBe(echoedUser.seq)
      const userRow = harness.page.locator(`[data-chat-anchor-key="${contextKey('input-message', echoedUser.data.id)}"]`)
      await waitUntil(() => userRow.count(), count => count === 1)
      expect(await userRow.getAttribute('data-chat-flow-kind')).toBe('user')
      expect(await userRow.textContent()).toContain(spec.userMarker)
      const assistant = turnEvents.find(event => event.type === 'assistant/message'
        && JSON.stringify(event.data.message?.content).includes(spec.doneMarker))
      if (assistant === undefined) throw new Error(`turn ${spec.index} has no durable final assistant response`)
      const assistantRow = harness.page.locator(`[data-chat-anchor-key="${contextKey('assistant-step', `${assistant.data.turn}:${assistant.data.step}`)}"]`)
      await waitUntil(() => assistantRow.count(), count => count === 1)
      expect(await assistantRow.getAttribute('data-chat-flow-kind')).toBe('assistant-step')
      expect(await assistantRow.textContent()).toContain(spec.doneMarker)

      if (spec.callId !== undefined && spec.toolMarker !== undefined) {
        const call = harness.page.locator(`[data-chat-call-id="${spec.callId}"]`)
        await call.waitFor({ timeout: 15_000 })
        expect(await call.textContent()).toContain(spec.toolMarker)
        const disclosure = call.locator('[data-sample="bash"]')
        expect(await disclosure.getAttribute('aria-expanded')).toBe('false')
        await disclosure.click()
        await waitUntil(() => disclosure.getAttribute('aria-expanded'), value => value === 'true')
        await call.getByText(spec.toolMarker, { exact: true }).last().waitFor()
        await disclosure.click()
        await waitUntil(() => disclosure.getAttribute('aria-expanded'), value => value === 'false')
      }
    }

    if (sessionId === undefined) throw new Error('continuous conversation completed no turn')
    const events = await sessionEvents(harness, sessionId)
    let priorEnd = -1
    for (const spec of specs) {
      const start = events.find(event => event.type === 'turn/start' && event.data?.turn === spec.index)?.seq
      const end = events.find(event => event.type === 'turn/end' && event.data?.turn === spec.index)?.seq
      if (start === undefined || end === undefined) throw new Error(`turn ${spec.index} has no durable boundary`)
      expect(start).toBeGreaterThan(priorEnd)
      priorEnd = end
      const turn = events.filter(event => event.seq >= start && event.seq <= end)
      const users = turn.filter(event => event.type === 'user/message' && event.data.source?.kind === 'user')
      const assistants = turn.filter(event => event.type === 'assistant/message')
      const final = assistants.filter(event => textBlocks(event.data.message.content).includes(spec.doneMarker))
      expect(users).toHaveLength(1)
      expect(textBlocks(users[0]!.data.content)).toBe(spec.prompt)
      expect(final).toHaveLength(1)
      expect(assistants).toHaveLength(spec.callId === undefined ? 1 : 2)
      const turnStarts = turn.filter(event => event.type === 'turn/start')
      expect(turnStarts).toHaveLength(1)
      expect(turnStarts[0]?.data.turn).toBe(spec.index)
      const turnEnds = turn.filter(event => event.type === 'turn/end')
      expect(turnEnds).toHaveLength(1)
      expect(turnEnds[0]?.data).toEqual({ turn: spec.index, reason: { kind: 'completed' } })
      expect(turn.filter(event => event.type === 'assistant/chunk')).toHaveLength(spec.deltas.length + (spec.callId === undefined ? 4 : 9))
      const calls = turn.filter(event => event.type === 'tool/call')
      const results = turn.filter(event => event.type === 'tool/result')
      expect(calls).toHaveLength(spec.callId === undefined ? 0 : 1)
      expect(results).toHaveLength(spec.callId === undefined ? 0 : 1)
      if (spec.toolMarker !== undefined) {
        expect(calls[0]?.data).toMatchObject({ turn: spec.index, callId: spec.callId, name: 'bash' })
        expect(results[0]?.data.turn).toBe(spec.index)
        expect(results[0]?.data.message.source.callId).toBe(spec.callId)
        expect(results[0]?.data.message.content[0].isError).toBe(false)
        expect(textBlocks(results[0]!.data.message.content[0].content)).toBe(`${spec.toolMarker}\n`)
      }
    }
    expect(events.filter(event => event.type === 'turn/end' && event.data.reason?.kind === 'completed')).toHaveLength(TURN_COUNT)
    expect(events.filter(event => event.type === 'assistant/chunk' && event.data.turn === TURN_COUNT).length).toBeGreaterThan(30)
    expect(specs.at(-1)!.prompt.length).toBeGreaterThan(4_000)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)
