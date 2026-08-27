import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, fixture, RustWebHarness } from './support'

interface EventData {
  readonly content?: Array<{ readonly text?: string; readonly type?: string }>
  readonly isError?: boolean
  readonly arguments?: unknown
  readonly header?: { readonly system?: string; readonly tools?: Array<{ readonly name: string }> }
  readonly name?: string
  readonly parentCallId?: string
  readonly rootCallId?: string
  readonly source?: { readonly kind?: string }
  readonly subCallId?: string
}

interface Event {
  readonly data: EventData
  readonly seq: number
  readonly type: string
}

const PROMPT = 'Using ONE run_code program: run bash `echo CODE_ROUND_OK`, then read the file missing.txt catching its error in the program. Return an object with both outcomes. Then reply DONE and stop.'
const DESCRIPTION = 'Run bash echo and catch missing file read'
const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/ptc-mode-round')
const UI_EXPECTED = join(SNAPSHOT_DIR, 'ui.expected.md')

test('PTC executes nested tools and renders durable sub-calls', async () => {
  const sourceFixture = await fixture('code-mode-round')
  const fixtureDocument = await readFile(sourceFixture, 'utf8')
  const fixturePrompts = fixtureDocument.trim().split('\n').flatMap(line => {
    const row = JSON.parse(line) as { type?: string; data?: { content?: Array<{ type?: string; text?: string }>; source?: { kind?: string } } }
    return row.type === 'user/message' && row.data?.source?.kind === 'user'
      ? row.data.content?.flatMap(block => block.type === 'text' && block.text !== undefined ? [block.text] : []) ?? []
      : []
  })
  expect(fixturePrompts).toEqual([PROMPT])
  const harness = await RustWebHarness.launch({
    name: 'ptc-mode-round-web-e2e',
    locale: 'en-US',
    agentMode: 'ptc',
    replayFixture: sourceFixture,
  })
  try {
    const composer = harness.page.locator('textarea:enabled').last()
    await composer.fill(PROMPT)
    const settled = harness.whenTurnSettled()
    await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
    const sessionId = await settled
    await harness.page.getByText('DONE', { exact: true }).last().waitFor({ timeout: 15_000 })
    const history = await harness.rpc<{ events: { event: Event }[] }>('session.history', {
      sessionId,
      maxMessages: 1_000,
    })
    expect(history.ok).toBe(true)
    const events = (history.value?.events ?? []).map(entry => entry.event)
    const users = events.filter(event => event.type === 'user/message' && event.data.source?.kind === 'user')
    expect(users).toHaveLength(1)
    expect(users[0]?.data.content?.[0]?.text).toBe(PROMPT)
    const headers = events.filter(event => event.type === 'request/header')
    expect(headers.length).toBeGreaterThan(0)
    for (const header of headers) {
      expect(header.data.header?.tools?.map(tool => tool.name)).toEqual(['run_code'])
      expect(header.data.header?.system).toContain('declare const tools')
    }
    const calls = events.filter(event => event.type === 'tool/call')
    expect(calls.map(event => event.data.name)).toEqual(['run_code'])
    const dispatches = events.filter(event => event.type === 'tool/code-dispatch')
    expect(dispatches.map(event => event.data.name).sort()).toEqual(['bash', 'read'])
    for (const dispatch of dispatches) {
      const { content, isError, parentCallId, rootCallId, subCallId } = dispatch.data
      if (parentCallId === undefined || rootCallId === undefined || subCallId === undefined) throw new Error('code dispatch lacks parent identity')
      expect(rootCallId).toBe(parentCallId)
      expect(subCallId.startsWith(`${parentCallId}:code:`)).toBe(true)
      expect(Array.isArray(content)).toBe(true)
      expect(typeof isError).toBe('boolean')
    }
    const bash = dispatches.find(event => event.data.name === 'bash')
    const read = dispatches.find(event => event.data.name === 'read')
    expect(bash?.data.isError).toBe(false)
    expect(JSON.stringify(bash?.data.content)).toContain('CODE_ROUND_OK')
    expect(read?.data.isError).toBe(true)
    expect(JSON.stringify(read?.data.content)).toContain('missing.txt')

    const codeRow = harness.page.locator('[data-variant="code"]').first()
    await codeRow.waitFor({ timeout: 15_000 })
    expect(await codeRow.textContent()).toContain(DESCRIPTION)
    const nest = harness.page.locator('[data-subcalls]').first()
    await nest.waitFor({ timeout: 15_000 })
    expect(await nest.locator('[data-sample="bash"]').count()).toBeGreaterThanOrEqual(1)
    expect(await nest.locator('[data-state="error"]').count()).toBeGreaterThanOrEqual(1)

    const frame = harness.page.locator('[style*="grid-template-columns"]').first()
    expect(await frame.getAttribute('data-details-collapsed')).toBe('true')
    await nest.locator('[data-sample="bash"]').first().click()
    expect(await frame.getAttribute('data-details-collapsed')).toBe('true')
    const snapshot = (await captureStableAria(harness.page, '[class*="centerCol"]'))
      .split(harness.root).join('{{cwd}}')
    expect(snapshot).toBe((await readFile(UI_EXPECTED, 'utf8')).trim())
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['ui.expected.md'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)
