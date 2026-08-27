import { cp, readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness } from './support'

interface Event {
  readonly type: string
  readonly data: {
    readonly content?: unknown
    readonly header?: { readonly tools?: Array<{ readonly name: string }> }
    readonly message?: { readonly content?: Array<{ readonly isError?: boolean }> }
    readonly name?: string
  }
}

const PROMPT = 'Use the Composition tools to define, validate, run, inspect, and stop the supplied WASM fixture. Then reply COMPOSITION_ROUND_OK and stop.'
const TOOLS = [
  'composition_define',
  'composition_validate',
  'composition_run',
  'composition_inspect',
  'composition_stop',
] as const
const SNAPSHOT = join(import.meta.dir, 'snapshots/cordis-tool-round/ui.expected.md')
let packagePath = ''

function replayRecording(): string {
  if (packagePath === '') throw new Error('WASM fixture path was not prepared')
  const calls = [
    {
      name: 'composition_define',
      arguments: {
        descriptor: {
          id: 'browser-wasm-fixture',
          entry: { runtime: 'wasm', package: packagePath },
          config: {},
        },
      },
    },
    { name: 'composition_validate', arguments: { id: 'browser-wasm-fixture' } },
    { name: 'composition_run', arguments: { id: 'browser-wasm-fixture' } },
    { name: 'composition_inspect', arguments: { id: 'browser-wasm-fixture' } },
    { name: 'composition_stop', arguments: { id: 'browser-wasm-fixture' } },
  ]
  let seq = 0
  const rows: unknown[] = [
    { type: 'session', version: 0, id: 'composition-mode-replay', createdAt: 0, cwd: '/workspace' },
  ]
  for (const [index, call] of calls.entries()) {
    const id = `composition-${index + 1}`
    const args = JSON.stringify(call.arguments)
    for (const chunk of [
      { type: 'block-start', index: 0, blockType: 'tool-call' },
      { type: 'tool-call-delta', index: 0, id, name: call.name, argumentsDelta: args },
      { type: 'block-end', index: 0, block: { type: 'tool-call', id, name: call.name, arguments: args } },
      { type: 'finish', reason: { kind: 'tool-calls' } },
    ]) {
      rows.push({ type: 'assistant/chunk', seq: seq++, time: 0, data: { turn: 1, step: index + 1, chunk } })
    }
  }
  for (const chunk of [
    { type: 'block-start', index: 0, blockType: 'text' },
    { type: 'text-delta', index: 0, text: 'COMPOSITION_ROUND_OK' },
    { type: 'block-end', index: 0, block: { type: 'text', text: 'COMPOSITION_ROUND_OK' } },
    { type: 'finish', reason: { kind: 'stop' } },
  ]) {
    rows.push({ type: 'assistant/chunk', seq: seq++, time: 0, data: { turn: 1, step: calls.length + 1, chunk } })
  }
  return rows.map(row => JSON.stringify(row)).join('\n')
}

test('Composition mode executes a real declarative WASM lifecycle', async () => {
  const harness = await RustWebHarness.launch({
    name: 'composition-mode-round-web-e2e',
    locale: 'en-US',
    agentMode: 'composition',
    beforeStart: async candidate => {
      packagePath = join(candidate.workspace, 'wasm-fixture')
      await cp(join(import.meta.dir, '../../fixtures/wasm/rust-minimal'), packagePath, { recursive: true })
    },
    replayRecording,
  })
  try {
    const composer = harness.page.locator('textarea:enabled').last()
    await composer.fill(PROMPT)
    const settled = harness.whenTurnSettled()
    await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
    const sessionId = await settled
    await harness.page.getByText('COMPOSITION_ROUND_OK', { exact: true }).waitFor({ timeout: 15_000 })

    const history = await harness.rpc<{ events: Array<{ event: Event }> }>('session.history', {
      sessionId,
      maxMessages: 1_000,
    })
    expect(history.ok).toBe(true)
    const events = (history.value?.events ?? []).map(entry => entry.event)
    const headers = events.filter(event => event.type === 'request/header')
    expect(headers.length).toBeGreaterThan(0)
    for (const header of headers) {
      const names = header.data.header?.tools?.map(tool => tool.name) ?? []
      for (const name of TOOLS) expect(names).toContain(name)
    }
    const calls = events.filter(event => event.type === 'tool/call')
    expect(calls.map(event => event.data.name)).toEqual(TOOLS)
    const results = events.filter(event => event.type === 'tool/result')
    expect(results).toHaveLength(TOOLS.length)
    expect(results.every(event => event.data.message?.content?.[0]?.isError === false)).toBe(true)
    expect(JSON.stringify(results)).toContain('active')
    expect(JSON.stringify(results)).toContain('disposed')

    for (const name of TOOLS) {
      await harness.page.locator(`[data-tool="${name}"]`).waitFor({ timeout: 15_000 })
    }
    const snapshot = (await captureStableAria(harness.page, '[class*="centerCol"]'))
      .split(harness.root).join('{{root}}')
      .split(harness.workspace).join('{{workspace}}')
    expect(snapshot).toBe((await readFile(SNAPSHOT, 'utf8')).trim())
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)
