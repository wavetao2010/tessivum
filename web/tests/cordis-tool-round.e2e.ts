import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/cordis-tool-round')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const UI_EXPECTED = join(SNAPSHOT_DIR, 'ui.expected.md')
const CORDIS_TOOLS = ['cordis_inspect_self', 'cordis_define', 'cordis_run', 'cordis_stop'] as const
const PACKAGE_CODE = 'return { name: "snapshot-noop", apply(ctx) {} }'
const CLIENT_CODE = 'return { inject: ["slots"], apply(ctx) { ctx.slots.register('
  + '{ name: "shell.overlay", id: "snapshot-probe" }, '
  + '() => React.createElement("div", { "data-snapshot-probe": "loaded" })) } }'
const PROMPT = 'Use only Cordis tools. First call cordis_inspect_self with no arguments. '
  + 'Then call cordis_define with plugin kind "new", idPrefix "snap", name "snapshot noop", '
  + 'purpose "does nothing, for the snapshot", '
  + `code.host exactly ${JSON.stringify(PACKAGE_CODE)} and code.client exactly ${JSON.stringify(CLIENT_CODE)}. `
  + 'Read its returned pluginId and packageId, then call cordis_run with those exact IDs and mode "run". '
  + 'After the run request returns, reply exactly CORDIS_UI_READY and stop.'
const STOP_PROMPT = 'Use only Cordis tools. Call cordis_stop with pluginId "snap-1". '
  + 'After it succeeds, reply exactly CORDIS_UI_DONE and stop.'

type Event = { type: string; data: Record<string, unknown> }

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function stringField(value: unknown, field: string): string | undefined {
  const candidate = record(value) ? value[field] : undefined
  return typeof candidate === 'string' ? candidate : undefined
}

function resultCallId(event: Event): string | undefined {
  return stringField(event.data, 'callId')
    ?? stringField(record(event.data.message) ? event.data.message.source : undefined, 'callId')
}

function resultIsError(event: Event): boolean | undefined {
  if (typeof event.data.isError === 'boolean') return event.data.isError
  const message = record(event.data.message) ? event.data.message : undefined
  const content = Array.isArray(message?.content) ? message.content : []
  const block = content[0]
  return record(block) && typeof block.isError === 'boolean' ? block.isError : undefined
}

async function fixtureUserPrompts(): Promise<string[]> {
  return (await readFile(FIXTURE, 'utf8'))
    .trim()
    .split('\n')
    .map(line => JSON.parse(line) as { type?: string; data?: { content?: Array<{ type?: string; text?: string }> } })
    .filter(row => row.type === 'user/message')
    .flatMap(row => row.data?.content ?? [])
    .flatMap(block => block.type === 'text' && block.text !== undefined ? [block.text] : [])
}

async function events(harness: RustWebHarness, sessionId: string): Promise<Event[]> {
  const history = await harness.rpc<{ events: Array<{ event: Event }> }>('session.history', { sessionId, maxMessages: 1_000 })
  if (!history.ok || history.value === undefined) throw new Error(`session.history failed: ${JSON.stringify(history.error)}`)
  return history.value.events.map(entry => entry.event)
}

function assertCompleteCordisLifecycle(log: readonly Event[]): void {
  const turnEnd = log.findLast(event => event.type === 'turn/end')
  expect(record(turnEnd?.data.reason) ? turnEnd?.data.reason.kind : undefined).toBe('completed')

  const calls = log.filter(event => event.type === 'tool/call')
  expect(calls.map(event => event.data.name)).toEqual(CORDIS_TOOLS)
  const callIds = new Set(calls.flatMap(event => [stringField(event.data, 'callId')]).filter((id): id is string => id !== undefined))
  const results = log.filter(event => event.type === 'tool/result' && callIds.has(resultCallId(event) ?? ''))
  expect(results).toHaveLength(CORDIS_TOOLS.length)
  expect(results.every(event => resultIsError(event) === false)).toBe(true)
}

test('Cordis tools define, approve, run, and stop their browser-half plugin', async () => {
  expect(await fixtureUserPrompts()).toEqual([PROMPT, STOP_PROMPT])
  const harness = await RustWebHarness.launch({
    name: 'cordis-tool-round-web-e2e',
    locale: 'en-US',
    replayFixture: FIXTURE,
    env: { TESSIVUM_CORDIS_TOOLS: '1', TESSIVUM_REPLAY_PACE_MS: '15' },
  })
  try {
    const input = harness.page.locator('textarea').first()
    await input.waitFor({ timeout: 10_000 })
    const runTurnSettled = harness.whenTurnSettled()
    await input.fill(PROMPT)
    await input.press('Enter')

    const approve = harness.page.locator('[data-cordis-approve]').first()
    await approve.waitFor({ timeout: 90_000 })
    expect(await harness.page.locator('[data-snapshot-probe]').count()).toBe(0)
    await approve.click()
    await waitUntil(() => harness.page.locator('[data-snapshot-probe]').count(), count => count === 1, 30_000)

    const sessionId = await runTurnSettled
    const stopTurnSettled = harness.whenTurnSettled()
    await input.fill(STOP_PROMPT)
    await input.press('Enter')
    await stopTurnSettled

    const log = await waitUntil(
      () => events(harness, sessionId),
      current => current.filter(event => event.type === 'tool/call').length === CORDIS_TOOLS.length
        && current.filter(event => event.type === 'turn/end').length === 2,
      30_000,
    )
    assertCompleteCordisLifecycle(log)

    await harness.page.getByText('CORDIS_UI_DONE', { exact: true }).waitFor({ timeout: 15_000 })
    const inspectRow = harness.page.locator('[data-tool="cordis_inspect_self"]').filter({ hasText: 'Inspect' }).first()
    await inspectRow.waitFor({ timeout: 10_000 })
    const defineRow = harness.page.locator('[data-tool="cordis_define"]').filter({ hasText: 'Cordis Plugin' }).first()
    await defineRow.waitFor({ timeout: 10_000 })
    await defineRow.locator('[aria-expanded]').first().click()
    await waitUntil(() => defineRow.textContent(), text => text?.includes('data-snapshot-probe') === true, 10_000)
    await defineRow.getByRole('tab', { name: 'Host' }).click()
    await waitUntil(() => defineRow.textContent(), text => text?.includes(PACKAGE_CODE) === true, 10_000)

    const runRow = harness.page.locator('[data-tool="cordis_run"]').filter({ hasText: 'Run Cordis Plugin' }).first()
    await runRow.waitFor({ timeout: 10_000 })
    await waitUntil(() => runRow.textContent(), text => text?.includes('snap-') === true, 10_000)
    const stopRow = harness.page.locator('[data-tool="cordis_stop"]').filter({ hasText: 'Stop Cordis Plugin' }).first()
    await stopRow.waitFor({ timeout: 10_000 })
    await waitUntil(() => stopRow.textContent(), text => text?.includes('snap-') === true, 10_000)
    expect(await stopRow.getAttribute('data-state')).toBe('ok')
    await waitUntil(() => harness.page.locator('[data-snapshot-probe]').count(), count => count === 0, 15_000)
    expect(await captureStableAria(harness.page, '[class*="centerCol"]')).toBe((await readFile(UI_EXPECTED, 'utf8')).trim())
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['session.jsonl', 'ui.expected.md'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 200_000)
