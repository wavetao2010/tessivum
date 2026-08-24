import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, openSessionByMarker, RustWebHarness, settledRecording, textReplay, waitUntil, withSubagents } from './support'

const PARENT = 'subagent-conversation-parent'
const CHILD = 'subagent-conversation-child'
const ONE_SHOT = 'subagent-conversation-one-shot'
const GRANDCHILD = 'subagent-conversation-grandchild'
const PARENT_PROMPT = 'Ask a research subagent to explain event sourcing.'
const INITIAL = 'Explain event sourcing in one sentence.'
const FOLLOW_UP = 'Now give the same explanation to a human reader.'
const POST_FORK_FOLLOW_UP = 'Continue the original conversation after the fork.'
const NESTED = 'Give one concrete event sourcing example.'
const LABEL = 'event-sourcing researcher'
const ONE_SHOT_LABEL = 'event-sourcing reviewer'
const NESTED_LABEL = 'example editor'
const PARENT_DONE = 'PARENT_READY'
const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/subagent-conversation')
const ANSWER = "Event sourcing is a pattern where all changes to an application's state are stored as an immutable, append-only sequence of events, rather than persisting only the current state, enabling full auditability, temporal queries, and event-driven architectures."
const REASONING = "The user is asking for a one-sentence description of event sourcing. This is a straightforward knowledge question that doesn't require any skill loading or tool calls."

type ChildEvent = {
  type: string
  data: {
    content?: Array<{ text?: string }>
    message?: { content?: Array<{ text?: string }> }
    reason?: { kind?: string }
    source?: { kind?: string }
  }
}

function fullAnswerReplay(requestId: string): string {
  return JSON.stringify({
    sessionId: CHILD,
    provider: 'recorded',
    model: 'recorded',
    requestId,
    chunks: [
      { type: 'block-start', index: 0, blockType: 'reasoning' },
      { type: 'reasoning-delta', index: 0, text: REASONING },
      { type: 'block-start', index: 1, blockType: 'text' },
      { type: 'text-delta', index: 1, text: ANSWER },
      { type: 'block-end', index: 0, block: { type: 'reasoning', text: REASONING } },
      { type: 'block-end', index: 1, block: { type: 'text', text: ANSWER } },
      { type: 'usage', usage: { inputTokens: 110, outputTokens: 79, cacheReadTokens: 7_680, reasoningTokens: 31 } },
      { type: 'finish', reason: { kind: 'stop' } },
    ],
  })
}

function emptySubagentRecording(parentId: string, depth: number): string {
  return [
    { type: 'session', version: 0, id: '{{sessionId}}', createdAt: 1_785_000_000_000, cwd: '{{cwd}}', parentSession: parentId, origin: 'subagent', delegationDepth: depth, agentPreset: 'standard' },
    { type: 'session/title', seq: 0, time: 1_785_000_000_001, data: { title: 'Explain event sourcing in one', messageSeqs: [], source: { kind: 'fallback' } } },
  ].map(row => JSON.stringify(row)).join('\n') + '\n'
}

function readOnlySubagentRecording(parentId: string, prompt: string): string {
  const time = 1_785_000_010_000
  return [
    { type: 'session', version: 0, id: '{{sessionId}}', createdAt: time, cwd: '{{cwd}}', parentSession: parentId, origin: 'subagent', delegationDepth: 2 },
    { type: 'turn/start', seq: 0, time, data: { turn: 1, trigger: { kind: 'message', source: { kind: 'user' } } } },
    { type: 'user/message', seq: 1, time: time + 1, data: { id: 'nested-user', role: 'user', content: [{ type: 'text', text: prompt }], source: { kind: 'user' } }, surfaceOp: 'append' },
    { type: 'turn/end', seq: 2, time: time + 3, data: { turn: 1, reason: { kind: 'completed' } } },
  ].map(row => JSON.stringify(row)).join('\n') + '\n'
}

function freshSettledRecording(title: string, user: string, assistant: string): string {
  const at = Date.now()
  const events = settledRecording(title, user, assistant).trimEnd().split('\n')
    .map(line => JSON.parse(line) as Record<string, unknown>)
  for (const event of events) event.time = at + Number(event.seq ?? 0)
  events[0].createdAt = at
  return `${events.map(event => JSON.stringify(event)).join('\n')}\n`
}

function agedOneShotRecording(): string {
  const duration = 192 * 24 * 60 * 60 * 1_000
  const at = Date.now() - duration
  return [
    { type: 'session', version: 0, id: '{{sessionId}}', createdAt: at, cwd: '{{cwd}}', origin: 'subagent', parentSession: PARENT, delegationDepth: 1 },
    { type: 'turn/start', seq: 0, time: at, data: { turn: 1, trigger: { kind: 'message', source: { kind: 'user' } } } },
    { type: 'user/message', seq: 1, time: at + 1, data: { id: 'one-shot-user', role: 'user', content: [{ type: 'text', text: 'Review the event sourcing explanation.' }], source: { kind: 'user' } }, surfaceOp: 'append' },
    { type: 'subagent/descriptor', seq: 2, time: at + 2, data: { version: 1, mode: 'one-shot', provider: 'native', label: ONE_SHOT_LABEL }, ignorable: true },
    { type: 'turn/end', seq: 3, time: at + duration, data: { turn: 1, reason: { kind: 'completed' } } },
  ].map(row => JSON.stringify(row)).join('\n') + '\n'
}

function childHistory(harness: RustWebHarness) {
  return harness.rpc<{ events: Array<{ event: ChildEvent }> }>('subagent.history', {
    parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', maxMessages: 100,
  })
}

async function assertGolden(harness: RustWebHarness, selector: string, name: string): Promise<void> {
  expect(`${await captureStableAria(harness.page, selector)}\n`).toBe(await readFile(join(SNAPSHOT_DIR, name), 'utf8'))
}

test('subagent catalog preserves cold hierarchy, transcript, fork placement, and resumed follow-ups', async () => {
  const harness = await RustWebHarness.launch({
    name: 'subagent-conversation',
    locale: 'en-US',
    replayRecording: [
      textReplay(PARENT, PARENT_DONE),
      fullAnswerReplay('child-1'),
      fullAnswerReplay('child-2'),
      fullAnswerReplay('child-3'),
    ].join('\n'),
    beforeStart: async candidate => {
      await candidate.seedSession(PARENT, withSubagents(PARENT, freshSettledRecording('Ask a research subagent to', PARENT_PROMPT, PARENT_DONE), [
        { childId: ONE_SHOT, label: ONE_SHOT_LABEL, mode: 'one-shot' },
        { childId: CHILD, label: LABEL, mode: 'continuable' },
      ]))
      await candidate.seedSession(CHILD, withSubagents(CHILD, emptySubagentRecording(PARENT, 1), [
        { childId: GRANDCHILD, label: NESTED_LABEL, mode: 'continuable' },
      ]))
      await candidate.seedSession(ONE_SHOT, agedOneShotRecording())
      await candidate.seedSession(GRANDCHILD, readOnlySubagentRecording(CHILD, NESTED))
    },
  })
  try {
    const apiCalls: string[] = []
    harness.page.on('request', request => apiCalls.push(new URL(request.url()).pathname))
    await openSessionByMarker(harness, PARENT_PROMPT, PARENT_DONE)
    const rootCatalog = await harness.rpc<{ entries: Array<{ id: string; mode: string; hasChildren: boolean }>; parentAvailable: boolean }>('subagent.list', { parentSessionId: PARENT })
    expect(rootCatalog).toMatchObject({ ok: true, value: { parentAvailable: true, entries: [
      { id: CHILD, mode: 'continuable', hasChildren: true },
      { id: ONE_SHOT, mode: 'one-shot', hasChildren: false },
    ] } })
    const nestedCatalog = await harness.rpc<{ entries: Array<{ id: string }> }>('subagent.list', { parentSessionId: CHILD })
    expect(nestedCatalog).toMatchObject({ ok: true, value: { entries: [{ id: GRANDCHILD }] } })

    const parentInput = harness.page.locator('textarea:enabled').last()
    await parentInput.fill('Keep the parent available.')
    const parentSettled = harness.whenTurnSettled()
    await parentInput.press('Enter')
    await parentSettled
    const initial = await harness.rpc('subagent.prompt', {
      parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable', content: [{ type: 'text', text: INITIAL }], clientTimeZone: 'Asia/Shanghai',
    })
    expect(initial.ok).toBe(true)
    await waitUntil(() => childHistory(harness), result => result.ok && result.value?.events.some(({ event }) => event.type === 'assistant/message' && event.data.message?.content?.some(block => block.text === ANSWER)) === true && result.value.events.filter(({ event }) => event.type === 'turn/end').length === 1, 30_000)

    const catalogPattern = '**/api/subagent.list'
    let firstClaimed = false
    let emptyDelivered = false
    let trailingRequested = false
    let releaseCatalog = (): void => {}
    const catalogHeld = new Promise<void>(resolve => { releaseCatalog = resolve })
    await harness.page.route(catalogPattern, async route => {
      if (firstClaimed) {
        const response = await route.fetch()
        trailingRequested = true
        await catalogHeld
        await route.fulfill({ response })
        return
      }
      firstClaimed = true
      const response = await route.fetch()
      const body = await response.json() as { result: { ok: true; value: { entries: unknown[] } } | { ok: false } }
      if (body.result.ok) body.result.value.entries = []
      await route.fulfill({ response, json: body })
      emptyDelivered = true
    })
    try {
      await harness.page.reload({ waitUntil: 'load' })
      await waitUntil(() => Promise.resolve(emptyDelivered), Boolean, 15_000)
      await harness.page.getByRole('button', { name: '3 subagents' }).waitFor()
      await harness.page.getByRole('button', { name: '3 subagents' }).click()
      await waitUntil(() => Promise.resolve(trailingRequested), Boolean, 15_000)
      const tree = harness.page.getByRole('tree', { name: 'Subagent sessions' })
      await tree.getByRole('treeitem', { name: 'Loading subagents' }).first().waitFor()
      expect(await tree.getByRole('treeitem', { name: 'Loading subagents' }).count()).toBe(2)
      await assertGolden(harness, '[role="tree"][aria-label="Subagent sessions"]', 'stale-catalog.expected.md')
      releaseCatalog()
      await tree.getByRole('treeitem', { name: new RegExp(LABEL) }).waitFor()
      await tree.press('Escape')
    } finally {
      releaseCatalog()
      await harness.page.unroute(catalogPattern)
    }

    await harness.page.getByRole('button', { name: '3 subagents' }).click()
    expect(await harness.page.getByRole('button', { name: `Expand ${ONE_SHOT_LABEL} descendants` }).count()).toBe(0)
    const oneShotRow = harness.page.getByRole('treeitem', { name: new RegExp(ONE_SHOT_LABEL) })
    expect(await oneShotRow.getByText('~6mo 12d', { exact: true }).count()).toBe(1)
    expect(await oneShotRow.getAttribute('aria-label')).toContain('192d 00h 00m 00s')
    await harness.page.getByRole('button', { name: `Expand ${LABEL} descendants` }).click()
    const childRow = harness.page.getByRole('treeitem', { name: new RegExp(LABEL) })
    const childLabel = await childRow.getAttribute('aria-label')
    await harness.page.waitForTimeout(1_100)
    expect(await childRow.getAttribute('aria-label')).toBe(childLabel)
    await harness.page.getByRole('treeitem', { name: new RegExp(NESTED_LABEL) }).waitFor()
    await assertGolden(harness, '[role="tree"][aria-label="Subagent sessions"]', 'tree.expected.md')
    await harness.page.getByRole('tree', { name: 'Subagent sessions' }).press('Escape')

    const callsBeforeOpen = apiCalls.filter(path => path === '/api/subagent.prompt').length
    await harness.page.getByRole('button', { name: '3 subagents' }).click()
    await harness.page.getByRole('treeitem', { name: new RegExp(LABEL) }).click()
    await harness.page.getByText(INITIAL, { exact: true }).waitFor()
    expect(apiCalls.filter(path => path === '/api/subagent.prompt')).toHaveLength(callsBeforeOpen)
    await harness.page.getByRole('navigation', { name: 'Session hierarchy' }).getByRole('button', { name: LABEL, disabled: true }).waitFor()
    await assertGolden(harness, '[role="tree"][aria-label="Sessions"]', 'sidebar.expected.md')

    const input = harness.page.getByRole('textbox', { name: 'Message the agent' })
    const continued = harness.page.waitForResponse(response => new URL(response.url()).pathname === '/api/subagent.prompt')
    await input.fill(FOLLOW_UP)
    await input.press('Enter')
    expect((await (await continued).json() as { result: { ok: boolean } }).result).toMatchObject({ ok: true })
    await waitUntil(() => childHistory(harness), result => result.ok && result.value?.events.filter(({ event }) => event.type === 'assistant/message' && event.data.message?.content?.some(block => block.text === ANSWER)).length === 2 && result.value.events.filter(({ event }) => event.type === 'turn/end').length === 2, 30_000)
    const followed = await childHistory(harness)
    if (!followed.ok || followed.value === undefined) throw new Error(JSON.stringify(followed.error))
    const followedEvents = followed.value.events.map(({ event }) => event)
    expect(followedEvents.flatMap(event => event.type === 'user/message' && event.data.source?.kind === 'user' ? event.data.content?.flatMap(block => block.text === undefined ? [] : [block.text]) ?? [] : [])).toEqual([INITIAL, FOLLOW_UP])
    expect(followedEvents.filter(event => event.type === 'turn/end').map(event => event.data.reason?.kind)).toEqual(['completed', 'completed'])
    expect(await harness.page.getByRole('button', { name: 'Stop generating' }).count()).toBe(0)
    await waitUntil(() => harness.page.getByText(FOLLOW_UP, { exact: true }).count(), count => count === 1, 15_000)
    await assertGolden(harness, '[class*="centerCol"]', 'ui.expected.md')
    await waitUntil(
      () => harness.rpc<{ parentAvailable: boolean }>('subagent.list', { parentSessionId: CHILD }),
      result => result.ok && result.value?.parentAvailable === false,
      30_000,
    )

    await harness.page.getByRole('button', { name: '1 subagent' }).click()
    const childTree = harness.page.getByRole('tree', { name: 'Subagent sessions' })
    const nestedRow = childTree.getByRole('treeitem', { name: new RegExp(NESTED_LABEL) })
    expect(await nestedRow.locator(':scope > *').count()).toBe(1)
    const [treeBox, clickAreaBox] = await Promise.all([childTree.boundingBox(), nestedRow.locator(':scope > *').boundingBox()])
    expect(treeBox).not.toBeNull()
    expect(clickAreaBox).not.toBeNull()
    expect([Math.round(clickAreaBox!.x - treeBox!.x), Math.round(treeBox!.x + treeBox!.width - clickAreaBox!.x - clickAreaBox!.width)]).toEqual([4, 4])
    await assertGolden(harness, '[role="tree"][aria-label="Subagent sessions"]', 'branchless.expected.md')
    await nestedRow.click()
    await harness.page.getByText('The parent session is offline; reopen it to continue sending messages.').waitFor()
    await harness.page.getByText(NESTED, { exact: true }).waitFor()
    const crumbs = await harness.page.getByRole('navigation', { name: 'Session hierarchy' }).getByRole('button').allTextContents()
    expect(crumbs.slice(-2)).toEqual([LABEL, NESTED_LABEL])
    await assertGolden(harness, '[class*="centerCol"]', 'nested.expected.md')

    await harness.page.getByRole('tree', { name: 'Sessions' }).getByRole('treeitem', { name: /Ask a research subagent to/ }).click()
    await harness.page.getByRole('button', { name: '3 subagents' }).click()
    await harness.page.getByRole('treeitem', { name: new RegExp(ONE_SHOT_LABEL) }).click()
    await harness.page.getByText('One-shot tasks do not accept follow-ups; review the full execution record here.').waitFor()

    await harness.page.getByRole('tree', { name: 'Sessions' }).getByRole('treeitem', { name: /Ask a research subagent to/ }).click()
    await harness.page.getByRole('button', { name: '3 subagents' }).click()
    await harness.page.getByRole('treeitem', { name: new RegExp(LABEL) }).click()
    const fork = harness.page.waitForResponse(response => new URL(response.url()).pathname === '/api/session.fork')
    await harness.page.getByRole('button', { name: 'Branch into a new conversation' }).last().click()
    expect((await (await fork).json() as { result: { ok: boolean } }).result).toMatchObject({ ok: true })
    await waitUntil(() => harness.page.getByRole('tree', { name: 'Sessions' }).getByRole('treeitem').count(), count => count === 3, 15_000)
    expect(await harness.page.getByText('Ungrouped', { exact: true }).count()).toBe(0)
    await waitUntil(() => harness.page.getByRole('navigation', { name: 'Session hierarchy' }).getByRole('button').count(), count => count === 1, 15_000)
    await assertGolden(harness, '[role="tree"][aria-label="Sessions"]', 'fork.expected.md')

    const sessions = harness.page.getByRole('tree', { name: 'Sessions' })
    await sessions.getByRole('treeitem', { name: /Ask a research subagent to/ }).click()
    await harness.page.getByRole('button', { name: '3 subagents' }).click()
    await harness.page.getByRole('treeitem', { name: new RegExp(LABEL) }).click()
    const secondFork = harness.page.waitForResponse(response => new URL(response.url()).pathname === '/api/session.fork')
    await harness.page.getByRole('button', { name: 'Branch into a new conversation' }).last().click()
    const forkReceipt = await (await secondFork).json() as { result: { ok: true; value: { sessionId: string } } | { ok: false } }
    if (!forkReceipt.result.ok) throw new Error('subagent fork was rejected')
    const forkId = forkReceipt.result.value.sessionId
    await waitUntil(
      () => harness.rpc<{ items: Array<{ sessionId: string; parentSessionId?: string; blank: boolean }> }>('session.list'),
      result => result.ok && result.value?.items.some(item => item.sessionId === forkId && item.parentSessionId === CHILD && item.blank === false) === true,
      30_000,
    )

    await sessions.getByRole('treeitem', { name: /Ask a research subagent to/ }).click()
    await harness.page.getByRole('button', { name: '3 subagents' }).click()
    await harness.page.getByRole('treeitem', { name: new RegExp(LABEL) }).click()
    const resumed = harness.page.waitForResponse(response => new URL(response.url()).pathname === '/api/subagent.prompt')
    const resumedInput = harness.page.getByRole('textbox', { name: 'Message the agent' })
    await resumedInput.fill(POST_FORK_FOLLOW_UP)
    await resumedInput.press('Enter')
    expect((await (await resumed).json() as { result: { ok: boolean } }).result).toMatchObject({ ok: true })
    const finalHistory = await waitUntil(() => childHistory(harness), result => result.ok && result.value?.events.some(({ event }) => event.type === 'user/message' && event.data.content?.some(block => block.text === POST_FORK_FOLLOW_UP)) === true && result.value.events.filter(({ event }) => event.type === 'turn/end').length === 3, 30_000)
    if (!finalHistory.ok || finalHistory.value === undefined) throw new Error(JSON.stringify(finalHistory.error))
    const resumedEvents = finalHistory.value.events.map(({ event }) => event)
    expect(resumedEvents.flatMap(event => event.type === 'user/message' && event.data.source?.kind === 'user' ? event.data.content?.flatMap(block => block.text === undefined ? [] : [block.text]) ?? [] : [])).toEqual([INITIAL, FOLLOW_UP, POST_FORK_FOLLOW_UP])
    expect(resumedEvents.filter(event => event.type === 'turn/end').map(event => event.data.reason?.kind)).toEqual(['completed', 'completed', 'completed'])
    const forkStillPresent = await harness.rpc<{ items: Array<{ sessionId: string; parentSessionId?: string; blank: boolean }> }>('session.list')
    if (!forkStillPresent.ok || forkStillPresent.value === undefined) throw new Error(JSON.stringify(forkStillPresent.error))
    expect(forkStillPresent.value.items).toContainEqual(expect.objectContaining({ sessionId: forkId, parentSessionId: CHILD, blank: false }))
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 240_000)
