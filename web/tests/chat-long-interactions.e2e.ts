import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { longChatFixture, openSessionByMarker, RustWebHarness, waitUntil } from './support'

const SESSION_ID = 'chat-long-interactions-e2e'
const CONTINUE_PROMPT = 'CHAT_INTERACTION_CONTINUE Continue from this exact branch point.'
const CONTINUE_FIRST = 'CHAT_INTERACTION_CONTINUE_FIRST'
const CONTINUE_DONE = 'CHAT_INTERACTION_CONTINUE_DONE'
const FIXTURE = longChatFixture({ markerPrefix: 'INTERACTION', title: 'Chat interaction fixture', turns: 88 })

type Event = { type: string; seq: number; data: any }
type SessionLog = { header: Record<string, any>; events: Event[] }

function contextKey(kind: string, id: string): string {
  return `${kind.length}:${kind}${id}`
}

function replayRecording(): string {
  const text = `${CONTINUE_FIRST} The branched conversation continues from the selected semantic boundary. ${CONTINUE_DONE}`
  return [
    { type: 'session', version: 0, id: 'chat-long-interactions-replay', createdAt: 0, cwd: '/workspace' },
    ...[
      { type: 'block-start', index: 0, blockType: 'text' },
      { type: 'text-delta', index: 0, text: `${CONTINUE_FIRST} ` },
      { type: 'text-delta', index: 0, text: `The branched conversation continues from the selected semantic boundary. ${CONTINUE_DONE}` },
      { type: 'block-end', index: 0, block: { type: 'text', text } },
      { type: 'usage', usage: { inputTokens: 256, outputTokens: 40 } },
      { type: 'finish', reason: { kind: 'stop' } },
    ].map((chunk, seq) => ({ type: 'assistant/chunk', seq, time: 0, data: { turn: 1, step: 1, chunk } })),
  ].map(row => JSON.stringify(row)).join('\n')
}

async function nextPaint(harness: RustWebHarness): Promise<void> {
  await harness.page.evaluate(async () => {
    await document.fonts.ready
    await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
  })
}

async function wheelUntilMounted(harness: RustWebHarness, selector: string): Promise<void> {
  const scrollport = harness.page.locator('[data-conversation-scroll]')
  const box = await scrollport.boundingBox()
  if (box === null) throw new Error('conversation scrollport has no layout box')
  await harness.page.mouse.move(box.x + box.width / 2, box.y + Math.min(140, box.height / 3))
  for (let attempt = 0; attempt < 20; attempt += 1) {
    if (await harness.page.locator(selector).count() > 0) return
    await harness.page.mouse.wheel(0, -1_100)
    await nextPaint(harness)
  }
  throw new Error(`semantic Chat target did not mount: ${selector}`)
}

async function readSession(harness: RustWebHarness, id: string): Promise<SessionLog> {
  const path = join(harness.dataDir, `session-${Buffer.from(id).toString('hex')}.jsonl`)
  const rows = (await readFile(path, 'utf8')).trim().split('\n').map(line => JSON.parse(line))
  return { header: rows[0], events: rows.slice(1) }
}

function carries(event: Event, marker: string): boolean {
  return JSON.stringify(event.data).includes(marker)
}

test('chat-long-interactions keeps virtualized rows and actions bound to semantic identities', async () => {
  const harness = await RustWebHarness.launch({
    name: 'chat-long-interactions-web-e2e',
    locale: 'en-US',
    viewport: { width: 1680, height: 900 },
    replayRecording: replayRecording(),
    beforeStart: candidate => candidate.seedSession(SESSION_ID, FIXTURE.log),
  })
  try {
    await openSessionByMarker(harness, FIXTURE.markers.user(1), FIXTURE.markers.assistant(88))

    const toolUserKey = contextKey('input-message', 'user-088')
    const toolAssistantKey = contextKey('assistant-step', '88:2')
    const callId1 = 'chat-scroll-088-1'
    const callId2 = 'chat-scroll-088-2'
    await wheelUntilMounted(harness, `[data-chat-call-id="${callId2}"]`)
    const toolUserRow = harness.page.locator(`[data-chat-anchor-key="${toolUserKey}"]`)
    const toolAssistantRow = harness.page.locator(`[data-chat-anchor-key="${toolAssistantKey}"]`)
    const call1 = harness.page.locator(`[data-chat-call-id="${callId1}"]`)
    const call2 = harness.page.locator(`[data-chat-call-id="${callId2}"]`)
    await waitUntil(() => toolUserRow.count(), count => count === 1)
    await waitUntil(() => toolAssistantRow.count(), count => count === 1)
    expect(await call1.count()).toBe(1)
    expect(await call2.count()).toBe(1)
    expect(await toolUserRow.getAttribute('data-chat-flow-kind')).toBe('user')
    expect(await toolAssistantRow.getAttribute('data-chat-flow-kind')).toBe('assistant-step')
    expect(await toolUserRow.textContent()).toContain(FIXTURE.markers.user(88))
    expect(await toolAssistantRow.textContent()).toContain(FIXTURE.markers.assistant(88))
    expect(await call1.textContent()).toContain(FIXTURE.markers.tool(88, 1))
    expect(await call2.textContent()).toContain(FIXTURE.markers.tool(88, 2))

    const expectedOrder = [
      toolUserKey,
      contextKey('tool-call', callId1),
      contextKey('tool-call', callId2),
      toolAssistantKey,
    ]
    const actualOrder = await harness.page.locator('[data-chat-anchor-key]').evaluateAll((rows, keys) => rows
      .map(row => (row as HTMLElement).dataset.chatAnchorKey)
      .filter((key): key is string => key !== undefined && keys.includes(key)), expectedOrder)
    expect(actualOrder).toEqual(expectedOrder)
    expect(await Promise.all([call1, call2].map(row => row.evaluate(element =>
      element.closest<HTMLElement>('[data-chat-flow-kind]')?.dataset.chatFlowKind ?? null,
    )))).toEqual(['tool-call', 'tool-call'])

    const summary1 = call1.locator('[data-sample="bash"]')
    const summary2 = call2.locator('[data-sample="bash"]')
    expect(await summary1.getAttribute('aria-expanded')).toBe('false')
    expect(await summary2.getAttribute('aria-expanded')).toBe('false')
    await summary2.focus()
    await summary2.press('Enter')
    await waitUntil(() => summary2.getAttribute('aria-expanded'), value => value === 'true')
    expect(await summary1.getAttribute('aria-expanded')).toBe('false')
    await call2.getByText(`${FIXTURE.markers.tool(88, 2)} output line 12`, { exact: true }).waitFor()

    const branchUserKey = contextKey('input-message', 'user-080')
    const branchAssistantKey = contextKey('assistant-step', '80:2')
    const turnTailKey = contextKey('turn-tail', '80')
    await wheelUntilMounted(harness, `[data-chat-anchor-key="${branchUserKey}"]`)
    const userRow = harness.page.locator(`[data-chat-anchor-key="${branchUserKey}"]`)
    const assistantRow = harness.page.locator(`[data-chat-anchor-key="${branchAssistantKey}"]`)
    const turnTailRow = harness.page.locator(`[data-chat-anchor-key="${turnTailKey}"]`)
    expect(await userRow.textContent()).toContain(FIXTURE.markers.user(80))
    expect(await assistantRow.textContent()).toContain(FIXTURE.markers.assistant(80))
    await harness.page.context().grantPermissions(['clipboard-read', 'clipboard-write'])
    await userRow.hover()
    await userRow.getByRole('button', { name: 'Copy', exact: true }).click()
    await waitUntil(() => harness.page.evaluate(() => navigator.clipboard.readText()),
      value => value === `${FIXTURE.markers.user(80)} Review the long-running conversation state for turn 80.`)

    const baseline = new Set((await harness.sessions()).map(session => session.sessionId))
    await turnTailRow.hover()
    await turnTailRow.getByRole('button', { name: 'Branch into a new conversation', exact: true }).click()
    const childId = await waitUntil(async () => (await harness.sessions()).find(session => !baseline.has(session.sessionId))?.sessionId,
      (value): value is string => value !== undefined)
    const source = await readSession(harness, SESSION_ID)
    const boundary = source.events.find(event => event.type === 'turn/end' && event.data.turn === 80)
    if (boundary === undefined) throw new Error('turn 80 has no durable boundary')
    const seededChild = await waitUntil(() => readSession(harness, childId),
      session => session.events.some(event => carries(event, FIXTURE.markers.assistant(80))))
    expect(seededChild.header.parentSession).toBe(SESSION_ID)
    expect(seededChild.header.seedLength).toBe(boundary.seq + 1)
    expect(seededChild.events.some(event => carries(event, FIXTURE.markers.assistant(80)))).toBe(true)
    expect(seededChild.events.some(event => carries(event, FIXTURE.markers.user(81)))).toBe(false)
    expect(seededChild.events.some(event => carries(event, FIXTURE.markers.user(88)))).toBe(false)

    const currentCrumb = harness.page.getByRole('navigation', { name: 'Session hierarchy' }).getByRole('button').last()
    await waitUntil(() => currentCrumb.textContent(), value => value === `${FIXTURE.title} (1)`)
    await harness.page.getByText(FIXTURE.markers.assistant(80), { exact: false }).last().waitFor()
    const composer = harness.page.locator('textarea:enabled').last()
    await composer.fill(CONTINUE_PROMPT)
    const settled = harness.whenTurnSettled()
    await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
    await waitUntil(() => harness.page.getByText(CONTINUE_PROMPT, { exact: true }).count(), count => count === 1)
    expect(await settled).toBe(childId)
    await harness.page.getByText(CONTINUE_DONE, { exact: false }).last().waitFor()
    await waitUntil(() => harness.page.locator('[data-streaming="true"]').count(), count => count === 0)
    expect(await composer.inputValue()).toBe('')
    expect(await composer.isEnabled()).toBe(true)

    const finalSource = await readSession(harness, SESSION_ID)
    const finalChild = await readSession(harness, childId)
    expect(finalSource.events.some(event => carries(event, CONTINUE_PROMPT))).toBe(false)
    expect(finalChild.events.filter(event => event.type === 'user/message' && carries(event, CONTINUE_PROMPT))).toHaveLength(1)
    expect(finalChild.events.findLast(event => event.type === 'turn/end')?.data.reason).toEqual({ kind: 'completed' })
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)
