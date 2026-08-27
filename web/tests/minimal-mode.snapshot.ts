import { expect, test } from 'bun:test'
import { RustWebHarness } from './support'

const PROMPT = 'Reply exactly MINIMAL_MODE_REQUEST_OK and stop.'
const FIXTURE = `${import.meta.dir}/snapshots/minimal-mode/session.jsonl`

interface RequestHeader {
  system?: string
  tools?: { name: string }[]
}

interface HistoryEvent {
  event: { type: string; data: { header?: RequestHeader } }
}

test('sends the exact Minimal Agent Mode prompt and schemas through the native agent', async () => {
  const harness = await RustWebHarness.launch({ name: 'minimal-mode-snapshot', replayFixture: FIXTURE })
  try {
    const workspaces = await harness.rpc<{ items: Array<{ workspaceId: string }> }>('workspace.list')
    const workspaceId = workspaces.value?.items[0]?.workspaceId
    if (workspaceId === undefined) throw new Error('native host did not publish a workspace')
    const created = await harness.rpc<{ sessionId: string }>('session.create', { sessionId: 'minimal-mode-smoke', workspaceId })
    const sessionId = created.value?.sessionId
    if (sessionId === undefined) throw new Error('native host did not create the minimal session')
    expect((await harness.rpc('agentPreset.select', { sessionId, agentPreset: 'minimal' })).ok).toBe(true)
    const accepted = await harness.rpc('session.prompt', {
      sessionId,
      mode: 'queue',
      content: [{ type: 'text', text: PROMPT }],
    })
    expect(accepted.ok).toBe(true)
    expect(await harness.whenTurnSettled()).toBe(sessionId)
    const history = await harness.rpc<{ events: HistoryEvent[] }>('session.history', { sessionId, maxMessages: 1_000 })
    expect(history.ok).toBe(true)
    const events = history.value?.events ?? []
    const header = events.find(({ event }) => event.type === 'request/header')?.event.data.header
    expect(header?.system).toBe('You are a helpful software engineer assistant.')
    expect(header?.tools?.map(tool => tool.name)).toEqual(['bash', 'str_replace_editor'])
    expect(events.some(({ event }) => event.type === 'assistant/message' || event.type === 'assistant/chunk')).toBe(true)
    harness.assertClean()
  } finally {
    await harness.close()
  }
})
