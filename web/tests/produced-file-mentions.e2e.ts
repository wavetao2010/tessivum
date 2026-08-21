import { expect, test } from 'bun:test'
import { openSeededSession, RustWebHarness, waitUntil } from './support'

const SESSION = 'produced-file-mentions-web-e2e'
const DONE = 'FILE_MENTION_DONE'
const WRITES = ['site/report.html', 'a/style.css', 'b/style.css'] as const

type Event = { type: string; data: Record<string, any> }
type HistoryEntry = { event: Event; view?: { for: string; view: Record<string, any> } }

function recording(): string {
  const time = 1_785_000_000_000
  let seq = 0
  const rows: Array<Record<string, unknown>> = [{ type: 'session', version: 0, id: '{{sessionId}}', createdAt: time, cwd: '{{cwd}}' }]
  const append = (type: string, data: unknown, surfaceOp?: string, sourceEventSeqs?: number[]): void => {
    rows.push({ type, time: time + seq + 1, seq: seq++, data, ...(sourceEventSeqs === undefined ? {} : { sourceEventSeqs }), ...(surfaceOp === undefined ? {} : { surfaceOp }) })
  }
  append('turn/start', { turn: 1 })
  append('user/message', {
    id: 'mention-user', role: 'user', content: [{ type: 'text', text: 'Write the report page and both stylesheets.' }], source: { kind: 'user' },
  }, 'append')
  append('session/title', { title: 'Produced file mentions', messageSeqs: [1], source: { kind: 'fallback' } })
  append('step/start', { turn: 1, step: 1 })
  const calls = WRITES.map((path, index) => ({
    path,
    callId: `file-mention-${index}`,
    arguments: JSON.stringify({ file_path: path, content: `content of ${path}\n` }),
  }))
  append('assistant/message', {
    id: 'mention-calls', role: 'assistant', turn: 1, step: 1,
    content: calls.map(call => ({ type: 'tool-call', id: call.callId, name: 'write', arguments: call.arguments })),
    source: { kind: 'model', provider: 'fixture', model: 'fixture' },
  }, 'append')
  for (const call of calls) {
    const callSeq = seq
    append('tool/call', { turn: 1, step: 1, callId: call.callId, name: 'write', arguments: call.arguments })
    append('tool/result', {
      turn: 1,
      step: 1,
      message: {
        id: `result-${call.callId}`,
        role: 'user',
        content: [{ type: 'tool-result', toolCallId: call.callId, content: [{ type: 'text', text: `Created ${call.path}` }], isError: false }],
        source: { kind: 'tool', callId: call.callId },
      },
      meta: { path: call.path, operation: 'create', diffs: [], locations: [{ path: call.path }], bytes: call.path.length + 12 },
    }, 'append', [callSeq])
  }
  append('step/end', { turn: 1, step: 1 })
  append('step/start', { turn: 1, step: 2 })
  append('assistant/message', {
    id: 'mention-done', role: 'assistant', turn: 1, step: 2,
    content: [{ type: 'text', text: `Wrote \`report.html\` plus two \`style.css\` copies; \`notes.md\` untouched.\n\n${DONE}` }],
    source: { kind: 'model', provider: 'fixture', model: 'fixture' },
  }, 'append')
  append('step/end', { turn: 1, step: 2 })
  append('turn/end', { turn: 1, reason: { kind: 'completed' } })
  return `${rows.map(row => JSON.stringify(row)).join('\n')}\n`
}

async function history(harness: RustWebHarness): Promise<HistoryEntry[]> {
  const result = await harness.rpc<{ events: HistoryEntry[] }>('session.history', { sessionId: SESSION, maxMessages: 1_000 })
  if (!result.ok || result.value === undefined) throw new Error(`session.history failed: ${JSON.stringify(result.error)}`)
  return result.value.events
}

test('only a unique produced-file inline-code basename is actionable', async () => {
  const harness = await RustWebHarness.launch({
    name: 'produced-file-mentions',
    beforeStart: candidate => candidate.seedSession(SESSION, recording()),
  })
  try {
    await openSeededSession(harness, DONE)
    const results = (await history(harness)).filter(entry => entry.event.type === 'tool/result')
    expect(results.map(entry => entry.view?.view.card)).toEqual(['diff', 'diff', 'diff'])
    expect(results.map(entry => entry.view?.view.locations)).toEqual(WRITES.map(path => [{ path }]))

    const mentions = harness.page.locator('[class*="markdown"] code button')
    expect(await waitUntil(() => mentions.count(), count => count === 1)).toBe(1)
    expect(await mentions.first().innerText()).toBe('report.html')
    expect(await mentions.first().getAttribute('aria-label')).toBe('Open site/report.html')
    expect(await mentions.first().getAttribute('title')).toBe('site/report.html')
    expect(await harness.page.getByText('Produced', { exact: true }).count()).toBe(1)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 90_000)
