import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, fixture, materializeRecording, openSeededSession, RustWebHarness, stableAria, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/seeded-history')
const UI_EXPECTED = join(SNAPSHOT_DIR, 'ui.expected.md')

const SEED_ID = 'seeded-history-web-e2e'
const PROMPT = 'Use the read tool twice in one assistant message: read a.txt and b.txt. Then reply with the single word DONE and stop.'

interface PersistedEvent {
  type: string
  seq?: number
  time?: number
  surfaceOp?: unknown
  data?: Record<string, unknown>
}

function withCompactionAndContext(raw: string): string {
  const rows = raw.trimEnd().split('\n').map(line => JSON.parse(line) as PersistedEvent)
  const header = rows[0] as PersistedEvent & { agentPreset?: string }
  if (header?.type === 'session') header.agentPreset = 'standard'
  const events = rows.slice(1)
  const surfaceSeqs = events
    .filter(event => event.surfaceOp === 'append' && ['user/message', 'assistant/message', 'tool/result'].includes(event.type))
    .flatMap(event => typeof event.seq === 'number' ? [event.seq] : [])
  const first = surfaceSeqs[0]
  const last = surfaceSeqs.at(-1)
  const lastTurn = events.filter(event => event.type === 'turn/end').at(-1)?.data?.turn
  const sequence = Math.max(...events.flatMap(event => typeof event.seq === 'number' ? [event.seq] : [-1])) + 1
  const time = Math.max(...events.flatMap(event => typeof event.time === 'number' ? [event.time] : [0])) + 1
  if (first === undefined || last === undefined || typeof lastTurn !== 'number') {
    throw new Error('seeded-history fixture lacks a closed rendered turn')
  }
  let seq = sequence
  let at = time
  const append = (type: string, data: Record<string, unknown>, surfaceOp?: unknown, sourceEventSeqs?: number[]): number => {
    const current = seq++
    rows.push({ type, seq: current, time: at++, data, ...(surfaceOp === undefined ? {} : { surfaceOp }), ...(sourceEventSeqs === undefined ? {} : { sourceEventSeqs }) })
    return current
  }
  const commandId = 'cmd-seeded-manual-compact'
  const compactionId = 'compact-seeded-manual-compact'
  append('command/run', { commandId, name: 'compact', args: '', source: { kind: 'user' } })
  const start = append('compaction/start', { compactionId, sourceCommandId: commandId, turn: null })
  const summary = append('compaction/summary', {
    compactionId,
    sourceCommandId: commandId,
    summary: [{ type: 'text', text: '## Cold resume compact summary\n\n- The exact summary remains available.' }],
    shadowedRange: { start: first, end: last },
    shadowedSeqs: surfaceSeqs,
    shadowedTokenCount: 1,
    provider: 'snapshot',
    model: 'snapshot-compactor',
  })
  append('user/message', {
    content: [{ type: 'text', text: '<context_checkpoint>Model-only compact checkpoint.</context_checkpoint>' }],
    source: { kind: 'plugin', plugin: 'compact', compactionId, sourceCommandId: commandId },
  }, { op: 'replace', start: 0, end: surfaceSeqs.length }, [start, summary, ...surfaceSeqs])
  append('compaction/end', { compactionId, sourceCommandId: commandId, turn: null })
  append('command/done', { commandId, kind: 'success', text: `Compacted ${surfaceSeqs.length} history items (~1 tokens).`, sourceEventSeq: summary })
  append('user/message', {
    content: [{
      type: 'text',
      text: '<system-reminder>\nThe following workspace instructions may be relevant to your work. Use them as guidance when applicable.\n\n'
        + Array.from({ length: 24 }, (_, index) => `Instruction ${index + 1}: preserve the logged context contract.`).join('\n')
        + '\n</system-reminder>',
    }],
    source: {
      kind: 'agent-instructions',
      form: 'instructions',
      baseline: true,
      changes: [{ action: 'set', scope: '.\u0000AGENTS.md', path: 'AGENTS.md', digest: 'context-injection-browser-snapshot' }],
    },
  }, 'append')
  append('user/message', {
    content: [{ type: 'text', text: 'Short injected context.' }],
    source: { kind: 'plugin', plugin: 'fixture' },
  }, 'append')
  append('turn/start', { turn: lastTurn + 1 })
  append('turn/end', { turn: lastTurn + 1, reason: { kind: 'completed' } })
  return `${rows.map(row => JSON.stringify(row)).join('\n')}\n`
}

function stableSeededAria(snapshot: string): string {
  return stableAria(snapshot)
    .replace(/(Compacted \d+ history items \(~)\d+( tokens\))/g, '$1{{tokens}}$2')
    .replaceAll(SEED_ID, '{{seededId}}')
    .replace(/\b[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\b/gi, '{{uuid}}')
}

async function seededHistory(harness: RustWebHarness): Promise<Array<{ event: { type: string; data: Record<string, unknown> } }>> {
  const history = await harness.rpc<{ events: Array<{ event: { type: string; data: Record<string, unknown> } }> }>('session.history', { sessionId: SEED_ID, maxMessages: 1_000 })
  if (!history.ok || history.value === undefined) throw new Error(`session.history failed: ${JSON.stringify(history.error)}`)
  return history.value.events
}

test('cold seeded history retains source transcript, compaction, context, command, and feedback browser contracts', async () => {
  const seedPath = await fixture('seeded-history', 'seed.jsonl')
  const raw = await readFile(seedPath, 'utf8')
  const harness = await RustWebHarness.launch({
    name: 'seeded-history',
    locale: 'en-US',
    beforeStart: async candidate => {
      const sessionWorkspace = join(candidate.workspace, 'workspace')
      await mkdir(sessionWorkspace, { recursive: true })
      await writeFile(join(sessionWorkspace, 'a.txt'), 'alpha\n')
      await writeFile(join(sessionWorkspace, 'b.txt'), 'beta\n')
      await candidate.seedSession(SEED_ID, materializeRecording(withCompactionAndContext(raw)))
    },
  })
  try {
    const direct = await harness.rpc<{ projections?: { asOfSeq: number; values: Record<string, unknown> } }>('session.history', { sessionId: SEED_ID })
    expect(direct.ok).toBe(true)
    const projections = direct.value?.projections
    expect(projections).toBeDefined()
    expect(projections?.asOfSeq).toBeGreaterThanOrEqual(0)
    expect(typeof projections?.values.title).toBe('string')
    expect(projections?.values).toHaveProperty('todos', null)
    const sessionStats = projections?.values.sessionStats as { turns: number; steps: number } | undefined
    expect(sessionStats).toBeDefined()
    expect(sessionStats?.turns).toBeGreaterThanOrEqual(1)
    expect(sessionStats?.steps).toBeGreaterThanOrEqual(sessionStats?.turns ?? 0)

    await openSeededSession(harness, 'DONE')
    await waitUntil(() => harness.page.getByText('compact', { exact: true }).count(), count => count === 1, 10_000)
    await waitUntil(() => harness.page.getByText(/^Compacted \d+ history items \(~\d+ tokens\)\.$/).count(), count => count === 1, 10_000)
    expect(await harness.page.getByText('Context compacted', { exact: true }).count()).toBe(1)
    const toolRows = harness.page.locator('[data-variant], [data-sample]')
    await waitUntil(() => toolRows.count(), count => count >= 2, 10_000)
    expect(await harness.page.getByText('a.txt', { exact: false }).count()).toBeGreaterThan(0)
    expect(await harness.page.getByText(PROMPT, { exact: true }).count()).toBe(1)
    await harness.page.getByRole('button', { name: 'Context injection tessivum-workspace-instructions', exact: true }).waitFor({ timeout: 10_000 })

    expect(stableSeededAria(await captureStableAria(harness.page, '[class*="centerCol"]'))).toBe(
      (await readFile(UI_EXPECTED, 'utf8')).trim(),
    )

    const disclosure = harness.page.getByRole('button', { name: 'Context injection tessivum-workspace-instructions', exact: true })
    expect(await disclosure.getAttribute('aria-expanded')).toBe('false')
    const collapsedIcon = disclosure.locator('svg').first()
    const collapsedIconBox = await collapsedIcon.boundingBox()
    expect(collapsedIconBox?.width).toBe(14)
    expect(collapsedIconBox?.height).toBe(14)
    await disclosure.click()
    await waitUntil(() => disclosure.getAttribute('aria-expanded'), value => value === 'true', 5_000)
    const body = disclosure.locator('..').locator('[data-context-injection-body]')
    await body.waitFor({ timeout: 5_000 })
    expect(await body.getAttribute('data-context-form')).toBeNull()
    expect(await body.locator('[data-context-files] li').allInnerTexts()).toEqual([])
    expect(await body.locator('[data-context-text]').innerText()).toContain('<system-reminder>')
    const headerBox = await disclosure.boundingBox()
    const bodyBox = await body.boundingBox()
    if (headerBox === null || bodyBox === null) throw new Error('context disclosure geometry is not measurable')
    expect(headerBox.height).toBe(24)
    expect(bodyBox.x - headerBox.x).toBe(22)
    expect(bodyBox.y - headerBox.y - headerBox.height).toBe(4)
    expect(bodyBox.height).toBe(141)
    expect(await body.evaluate(element => {
      const computed = getComputedStyle(element)
      return {
        backgroundColor: computed.backgroundColor,
        borderRadius: computed.borderRadius,
        color: computed.color,
        fontSize: computed.fontSize,
        lineHeight: computed.lineHeight,
        padding: [computed.paddingTop, computed.paddingRight, computed.paddingBottom, computed.paddingLeft],
        scrolls: element.scrollHeight > element.clientHeight,
      }
    })).toEqual({
      backgroundColor: 'rgb(249, 250, 251)', borderRadius: '8px', color: 'rgb(129, 133, 140)', fontSize: '11px', lineHeight: '16px', padding: ['10px', '16px', '12px', '12px'], scrolls: true,
    })
    await disclosure.click()
    await waitUntil(() => disclosure.getAttribute('aria-expanded'), value => value === 'false', 5_000)

    const fileLink = harness.page.locator('[data-variant="read"] button').first()
    await fileLink.waitFor({ timeout: 10_000 })
    const frame = harness.page.locator('[style*="grid-template-columns"]').first()
    expect(await frame.getAttribute('data-details-collapsed')).toBe('true')
    await fileLink.click()
    await waitUntil(() => frame.getAttribute('data-details-collapsed'), value => value === 'true', 5_000)
    await waitUntil(() => harness.page.getByText('a.txt', { exact: false }).count(), count => count > 0, 5_000)

    const marker = harness.page.getByRole('button', { name: /Context compacted Compacted \d+ history items/ })
    await marker.waitFor({ timeout: 10_000 })
    expect(await marker.getAttribute('aria-expanded')).toBe('false')
    await marker.click()
    await waitUntil(() => marker.getAttribute('aria-expanded'), value => value === 'true', 5_000)
    await waitUntil(() => harness.page.getByRole('heading', { name: 'Cold resume compact summary' }).count(), count => count === 1, 5_000)
    expect(await harness.page.getByText('The exact summary remains available.', { exact: false }).count()).toBeGreaterThan(0)
    await marker.click()
    await waitUntil(() => marker.getAttribute('aria-expanded'), value => value === 'false', 5_000)

    const composer = harness.page.locator('textarea:enabled').last()
    await composer.fill('/permission read-only')
    await composer.press('Enter')
    await harness.page.getByRole('button', { name: 'Access mode, current: Read Only' }).waitFor({ timeout: 10_000 })
    const row = harness.page.locator('[data-variant="others"]').filter({ hasText: 'preset read-only' })
    await waitUntil(() => row.count(), count => count === 1, 10_000)
    expect(await row.getByText('permission', { exact: true }).count()).toBe(1)
    expect(await row.getByText('/permission read-only', { exact: true }).count()).toBe(0)
    expect(stableSeededAria(await captureStableAria(harness.page, '[class*="centerCol"]'))).toBe(
      (await readFile(join(SNAPSHOT_DIR, 'command-row.expected.md'), 'utf8')).trim(),
    )

    await composer.fill('/feedback the diff view is unreadable')
    await composer.press('Enter')
    const feedback = harness.page.locator('[data-variant="others"]').filter({ hasText: `Feedback recorded for session ${SEED_ID}` })
    await feedback.waitFor({ timeout: 10_000 })
    const feedbackDisclosure = feedback.locator('[data-expandable]')
    expect(await feedbackDisclosure.getAttribute('aria-expanded')).toBe('false')
    await feedbackDisclosure.click()
    await waitUntil(() => feedbackDisclosure.getAttribute('aria-expanded'), value => value === 'true', 5_000)
    const done = (await seededHistory(harness)).map(({ event }) => event).filter(event => event.type === 'command/done').at(-1)
    if (done === undefined) throw new Error('feedback command did not settle')
    const [sessionLine, userLine, extraLine] = String(done.data.text ?? '').split('\n')
    expect(sessionLine).toBe(`Feedback recorded for session ${SEED_ID}`)
    expect(userLine).toMatch(/^Anonymous user: [0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\./i)
    expect(extraLine).toBeUndefined()
    expect(stableSeededAria(await captureStableAria(harness.page, '[class*="centerCol"]'))).toBe(
      (await readFile(join(SNAPSHOT_DIR, 'feedback-row.expected.md'), 'utf8')).trim(),
    )

    const shortDisclosure = harness.page.getByRole('button', { name: 'Context injection fixture', exact: true })
    await shortDisclosure.waitFor({ timeout: 10_000 })
    await shortDisclosure.click()
    await waitUntil(() => shortDisclosure.getAttribute('aria-expanded'), value => value === 'true', 5_000)
    const shortBody = harness.page.locator('[data-context-injection-body]:not([data-context-form])')
    const shortBox = await shortBody.boundingBox()
    if (shortBox === null) throw new Error('short context disclosure geometry is not measurable')
    expect(shortBox.height).toBeLessThan(141)
    expect(await shortBody.evaluate(element => element.scrollHeight > element.clientHeight)).toBe(false)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)
