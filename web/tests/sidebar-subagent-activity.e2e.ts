import { readFile } from 'node:fs/promises'
import { expect, test } from 'bun:test'
import { captureStableAria, fixture, RustWebHarness, settledRecording, subagentRecording, textReplay, waitUntil, withSubagents } from './support'

const PARENT = 'sidebar-subagent-activity-owner'
const CHILD = 'sidebar-subagent-activity-child'
const OWNER_PROMPT = 'Delegate a background job.'
const CHILD_PROMPT = 'Hold this delegated task open.'

function heldReplay(): string {
  return JSON.stringify({
    sessionId: CHILD,
    provider: 'recorded',
    model: 'recorded',
    chunks: [
      { type: 'block-start', index: 0, blockType: 'text' },
      ...Array.from({ length: 2_000 }, () => ({ type: 'text-delta', index: 0, text: 'x' })),
      { type: 'block-end', index: 0, block: { type: 'text', text: 'CHILD_DONE' } },
      { type: 'finish', reason: { kind: 'stop' } },
    ],
  })
}

test('sidebar pins a running direct child on its available owner', async () => {
  const harness = await RustWebHarness.launch({
    name: 'sidebar-subagent-activity',
    locale: 'en-US',
    env: { TESSIVUM_REPLAY_PACE_MS: '50' },
    replayRecording: [textReplay(PARENT, 'OWNER_DONE'), heldReplay()].join('\n'),
    beforeStart: async candidate => {
      await candidate.seedSession(PARENT, withSubagents(PARENT, settledRecording(OWNER_PROMPT, OWNER_PROMPT, 'OWNER_DONE'), [
        { childId: CHILD, label: 'sidebar activity child', mode: 'continuable' },
      ]))
      await candidate.seedSession(CHILD, subagentRecording(PARENT, 'sidebar activity child', CHILD_PROMPT, 'CHILD_DONE'))
    },
    beforePage: async candidate => {
      const parent = await candidate.rpc('session.prompt', {
        sessionId: PARENT, mode: 'queue', content: [{ type: 'text', text: 'Keep the owner available.' }],
      })
      if (!parent.ok) throw new Error(JSON.stringify(parent.error))
      const child = await candidate.rpc('subagent.prompt', {
        parentSessionId: PARENT, childSessionId: CHILD, mode: 'continuable',
        content: [{ type: 'text', text: CHILD_PROMPT }],
      })
      if (!child.ok) throw new Error(JSON.stringify(child.error))
      await waitUntil(
        () => candidate.rpc<{ entries: Array<{ id: string; activity: string }> }>('subagent.list', { parentSessionId: PARENT }),
        result => result.ok && result.value?.entries.some(entry => entry.id === CHILD && entry.activity === 'running') === true,
        10_000,
      )
    },
  })
  try {
    const catalog = await harness.rpc<{ entries: Array<{ id: string; activity: string; label: string }>; parentAvailable: boolean }>('subagent.list', { parentSessionId: PARENT })
    expect(catalog).toMatchObject({
      ok: true,
      value: { parentAvailable: true, entries: [{ id: CHILD, label: 'sidebar activity child', activity: 'running' }] },
    })
    const sessions = await harness.rpc<{ items: Array<{ sessionId: string; workspaceId?: string; blank: boolean }> }>('session.list')
    const parent = sessions.value?.items.find(item => item.sessionId === PARENT)
    const blank = sessions.value?.items.find(item => item.blank && item.sessionId !== PARENT)
    if (!sessions.ok || parent?.workspaceId === undefined || blank === undefined) throw new Error('sidebar activity workspace baseline is incomplete')
    expect((await harness.rpc('workspace.insertSessionBefore', {
      workspaceId: parent.workspaceId, sessionId: PARENT, beforeSessionId: blank.sessionId,
    })).ok).toBe(true)
    await harness.page.evaluate(({ workspaceId, parentId, blankId }) => localStorage.setItem('dsh.workspace.view.v5', JSON.stringify({
      groupBy: 'workspace', orderBy: 'manual', groupExpansion: { [workspaceId]: true },
      sessionOrderByAccount: { [workspaceId]: [parentId, blankId] }, sessionUpdatedAtByAccount: { [workspaceId]: {} },
    })), { workspaceId: parent.workspaceId, parentId: PARENT, blankId: blank.sessionId })
    await harness.page.reload({ waitUntil: 'load' })
    const sidebar = harness.page.getByRole('tree', { name: 'Sessions' })
    const ownerRow = sidebar.getByRole('treeitem', { name: /1 subagent running Delegate a background job/ })
    await ownerRow.waitFor({ timeout: 10_000 })
    expect(await captureStableAria(harness.page, '[role="tree"][aria-label="Sessions"]')).toBe(
      (await readFile(await fixture('sidebar-subagent-activity', 'owner-running.expected.md'), 'utf8')).trim(),
    )
    expect(await ownerRow.locator('[data-state="ongoing"]').count()).toBe(1)
    await ownerRow.click()
    const runningTrigger = harness.page.getByRole('button', { name: '1 subagent running' })
    await runningTrigger.waitFor({ timeout: 10_000 })
    expect(await runningTrigger.locator('[data-state="ongoing"]').count()).toBe(1)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 60_000)
