import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { openSeededSession, RustWebHarness, waitUntil } from './support'

const OVERLAY = join(import.meta.dir, 'produced-files.overlay.yml')
const SESSION = 'produced-files-web-e2e'
const DONE = 'PRODUCED_FILES_DONE'
const PRODUCED = [
  '关于我.md',
  'index.html',
  'long-generated-experience-specification-for-produced-files-overflow.md',
  'styles.css',
  'app.ts',
  'schema.json',
  'README.md',
  'preview.svg',
  'notes.txt',
  'manifest.yaml',
] as const

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
    id: 'produced-user', role: 'user', content: [{ type: 'text', text: 'Create the site files.' }], source: { kind: 'user' },
  }, 'append')
  append('session/title', { title: 'Produced files overflow', messageSeqs: [1], source: { kind: 'fallback' } })
  append('step/start', { turn: 1, step: 1 })
  const calls = PRODUCED.map((path, index) => ({
    path,
    callId: `produced-files-${index}`,
    arguments: JSON.stringify({ file_path: path, content: `content of ${path}\n` }),
  }))
  append('assistant/message', {
    id: 'produced-calls', role: 'assistant', turn: 1, step: 1,
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
    id: 'produced-done', role: 'assistant', turn: 1, step: 2,
    content: [{ type: 'text', text: `Created the site.\n\n${DONE}` }],
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

test('a completed write turn keeps a one-line ten-file summary and native folder handoff', async () => {
  expect(await readFile(OVERLAY, 'utf8')).toContain('nativeOpen: true')
  const harness = await RustWebHarness.launch({
    name: 'produced-files',
    viewport: { width: 1280, height: 900 },
    beforeStart: candidate => candidate.seedSession(SESSION, recording()),
  })
  try {
    await openSeededSession(harness, DONE)
    const entries = await history(harness)
    const calls = entries.filter(entry => entry.event.type === 'tool/call')
    const results = entries.filter(entry => entry.event.type === 'tool/result')
    expect(calls).toHaveLength(PRODUCED.length)
    expect(results).toHaveLength(PRODUCED.length)
    expect(calls.map(entry => entry.view?.view.card)).toEqual(Array(PRODUCED.length).fill('diff'))
    expect(calls.map(entry => entry.view?.view.diffs?.[0])).toEqual(PRODUCED.map(path => ({
      path, oldText: null, newText: `content of ${path}\n`,
    })))
    expect(results.map(entry => entry.view?.view.locations)).toEqual(PRODUCED.map(path => [{ path }]))

    await harness.page.setViewportSize({ width: 780, height: 900 })
    const row = harness.page.locator('[data-produced-files-row]')
    await row.waitFor({ timeout: 15_000 })
    const chips = row.getByRole('button')
    expect(await waitUntil(() => chips.count(), count => count === 2)).toBe(2)
    expect(await chips.nth(0).innerText()).toBe('关于我.md')
    expect(await chips.nth(1).innerText()).toBe('index.html')
    expect(await row.getByText('+ 8 files', { exact: true }).count()).toBe(1)
    expect(await harness.page.getByText('Produced', { exact: true }).count()).toBe(1)

    const showFolder = harness.page.getByRole('button', { name: 'Show in folder', exact: true })
    expect(await showFolder.count()).toBe(1)
    let opened: { payload?: unknown } | undefined
    await harness.page.route('**/api/host.openPath', async route => {
      const request = route.request().postDataJSON() as { rpcId?: string; payload?: unknown }
      opened = request
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ type: 'server-response', rpcId: request.rpcId, result: { ok: true, value: { opened: true } } }),
      })
    })
    try {
      const [response] = await Promise.all([
        harness.page.waitForResponse(value => new URL(value.url()).pathname === '/api/host.openPath'),
        showFolder.click({ clickCount: 1 }),
      ])
      expect(response.status()).toBe(200)
      expect(opened?.payload).toEqual({ path: `${harness.workspace}/.` })
    } finally {
      await harness.page.unroute('**/api/host.openPath')
    }

    const tops = await row.locator(':scope > *').evaluateAll(elements => elements.map(element => element.getBoundingClientRect().top))
    expect(new Set(tops.map(top => Math.round(top))).size).toBe(1)
    const geometry = await row.evaluate(element => ({ clientWidth: element.clientWidth, scrollWidth: element.scrollWidth }))
    expect(geometry.scrollWidth).toBeLessThanOrEqual(geometry.clientWidth)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
