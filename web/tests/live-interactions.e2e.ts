import { existsSync } from 'node:fs'
import { mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, describe, expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/live-interactions')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const PROMPT = 'Reply with a one-sentence description of event sourcing, then stop.'
const AUTH_PROVIDER_MESSAGE = 'Authentication Fails, Your api key: sk-preview-secret is invalid'

let harness: RustWebHarness | undefined
let sidecarDir: string | undefined

async function assertGolden(selector: string, file: string): Promise<void> {
  if (harness === undefined) throw new Error('harness is not running')
  expect(await captureStableAria(harness.page, selector)).toBe((await Bun.file(join(SNAPSHOT_DIR, file)).text()).trim())
}

async function fixtureUserPrompts(): Promise<string[]> {
  return (await readFile(FIXTURE, 'utf8'))
    .trim()
    .split('\n')
    .map(line => JSON.parse(line) as { type?: string; data?: { content?: Array<{ type?: string; text?: string }> } })
    .filter(row => row.type === 'user/message')
    .flatMap(row => row.data?.content ?? [])
    .filter(block => block.type === 'text')
    .flatMap(block => block.text === undefined ? [] : [block.text])
}

async function derivedSuccess(): Promise<{ kind: 'chunks'; chunks: unknown[] }> {
  const chunks = (await readFile(FIXTURE, 'utf8'))
    .trim()
    .split('\n')
    .flatMap(line => {
      const row = JSON.parse(line) as any
      if (row.type === 'assistant/chunk') return [{ seq: row.seq, chunk: row.data.chunk }]
      if (row.type !== 'reasoning-chunks' && row.type !== 'text-chunks') return []
      const kind = row.type === 'reasoning-chunks' ? 'reasoning-delta' : 'text-delta'
      return row.data.texts.map((text: string, index: number) => ({
        seq: row.seq0 + index,
        chunk: { type: kind, index: row.data.index, text },
      }))
    })
    .sort((left, right) => left.seq - right.seq)
    .map(row => row.chunk)
  expect(chunks.filter((chunk: any) => chunk.type === 'finish')).toHaveLength(1)
  return { kind: 'chunks', chunks }
}

async function sessionEvents(id: string): Promise<Array<{ type?: string; data?: { reason?: { kind?: string } } }>> {
  if (harness === undefined) throw new Error('harness is not running')
  const path = join(harness.dataDir, `session-${Buffer.from(id).toString('hex')}.jsonl`)
  return (await readFile(path, 'utf8')).trim().split('\n').slice(1).map(line => JSON.parse(line))
}

function turnEndReasons(events: Array<{ type?: string; data?: { reason?: { kind?: string } } }>): string[] {
  return events
    .filter(event => event.type === 'turn/end')
    .flatMap(event => event.data?.reason?.kind === undefined ? [] : [event.data.reason.kind])
}

async function launch(buildOverride?: (sidecarHome: string) => Promise<unknown> | unknown): Promise<void> {
  let env: Record<string, string> | undefined
  if (buildOverride !== undefined) {
    sidecarDir = await mkdtemp(join(tmpdir(), 'tessivum-live-interactions-'))
    const overridePath = join(sidecarDir, 'replay.override.json')
    await writeFile(overridePath, JSON.stringify(await buildOverride(sidecarDir)))
    env = { TESSIVUM_REPLAY_OVERRIDE_FILE: overridePath }
  }
  harness = await RustWebHarness.launch({
    name: 'live-interactions',
    locale: 'en-US',
    replayFixture: FIXTURE,
    env,
  })
}

async function sendPrompt(timeoutMs?: number): Promise<{ settled: Promise<string> }> {
  if (harness === undefined) throw new Error('harness is not running')
  const input = harness.page.locator('textarea').first()
  await input.waitFor({ timeout: 10_000 })
  const settled = harness.whenTurnSettled(timeoutMs)
  await input.fill(PROMPT)
  await input.press('Enter')
  return { settled }
}

describe('live interactions over RustWebHarness', () => {
  afterEach(async () => {
    const failures: unknown[] = []
    const closing = harness
    harness = undefined
    await closing?.close().catch(error => failures.push(error))
    if (sidecarDir !== undefined) await rm(sidecarDir, { recursive: true, force: true }).catch(error => failures.push(error))
    sidecarDir = undefined
    if (failures.length === 1) throw failures[0]
    if (failures.length > 1) throw new AggregateError(failures, 'live interactions teardown failed')
  })

  test('cancels a hung stream deterministically via the readyFile marker', async () => {
    expect(await fixtureUserPrompts()).toEqual([PROMPT])
    let marker = ''
    await launch(sidecarHome => {
      marker = join(sidecarHome, '.hang-ready')
      return { patches: [{ at: 0, entry: { kind: 'hang', readyFile: marker } }] }
    })
    const current = harness!
    const { settled } = await sendPrompt()
    await waitUntil(async () => existsSync(marker), value => value, 15_000)
    await waitUntil(
      () => current.page.getByRole('status').filter({ hasText: 'Deep diving...' }).isVisible(),
      value => value,
      10_000,
    )
    await assertGolden('[class*="centerCol"]', 'loading.expected.md')
    await current.page.getByRole('button', { name: 'Stop generating' }).click()
    const id = await settled
    expect(turnEndReasons(await sessionEvents(id)).at(-1)).toBe('aborted')
    await waitUntil(() => current.page.locator('textarea').first().isEnabled(), value => value, 10_000)
    await waitUntil(() => current.page.locator('[data-streaming="true"]').count(), count => count === 0, 10_000)
    await assertGolden('[class*="centerCol"]', 'cancel.expected.md')
    current.assertClean()
  }, 120_000)

  test('surfaces a non-retryable AUTH failure without retrying', async () => {
    await launch(() => ({
      patches: [{ at: 0, entry: { kind: 'throw', chunks: [], message: AUTH_PROVIDER_MESSAGE, code: 'AUTH' } }],
    }))
    const current = harness!
    const { settled } = await sendPrompt()
    const id = await settled
    const events = await sessionEvents(id)
    expect(turnEndReasons(events).at(-1)).toBe('error')
    expect(events.filter(event => event.type === 'llm/retry')).toHaveLength(0)
    await waitUntil(() => current.page.locator('textarea').first().isEnabled(), value => value, 10_000)
    expect(await current.page.locator('[data-streaming="true"]').count()).toBe(0)
    const errorStatus = current.page.getByRole('status').filter({ hasText: 'This turn failed' })
    await errorStatus.waitFor({ timeout: 10_000 })
    expect(await errorStatus.textContent()).toContain('API key is invalid')
    expect(await errorStatus.textContent()).toContain('AUTH')
    expect(await current.page.locator('body').textContent()).not.toContain('sk-preview-secret')
    await assertGolden('[class*="centerCol"]', 'error-auth.expected.md')
    await current.page.getByRole('tab', { name: 'Trajectory' }).click()
    const requestMarker = current.page.locator('tr[data-request-only="true"]').last().getByRole('button', { name: /Request #/ })
    await requestMarker.click()
    await current.page.getByText('API key is invalid', { exact: true }).waitFor({ timeout: 10_000 })
    expect(await current.page.locator('body').textContent()).not.toContain('sk-preview-secret')
    current.assertClean()
  }, 120_000)

  test('keeps a terminal request marker inside the trajectory table', async () => {
    await launch(() => ({
      patches: [{ at: 0, entry: { kind: 'throw', chunks: [], message: AUTH_PROVIDER_MESSAGE, code: 'AUTH' } }],
    }))
    const current = harness!
    const { settled } = await sendPrompt()
    await settled
    await current.page.getByRole('tab', { name: 'Trajectory' }).click()
    const tailRequest = current.page.locator('tr[data-request-only="true"]').last()
    const requestMarker = tailRequest.getByRole('button', { name: /Request #/ })
    await requestMarker.waitFor({ timeout: 10_000 })
    const markerWithinTable = await requestMarker.evaluate(element => {
      const marker = element.getBoundingClientRect()
      const table = element.closest('table')?.getBoundingClientRect()
      if (table === undefined) throw new Error('request marker has no table')
      return marker.bottom <= table.bottom
    })
    expect(markerWithinTable).toBe(true)
    current.assertClean()
  }, 120_000)

  test('recovers a transient SERVER failure through llm-retry and completes', async () => {
    const success = await derivedSuccess()
    await launch(() => ({
      patches: [
        { at: 0, entry: { kind: 'throw', chunks: [], message: 'upstream 503', code: 'SERVER' } },
        { at: 1, entry: success },
      ],
    }))
    const current = harness!
    const { settled } = await sendPrompt(60_000)
    const id = await settled
    const events = await sessionEvents(id)
    expect(turnEndReasons(events).at(-1)).toBe('completed')
    expect(events.filter(event => event.type === 'llm/retry').length).toBeGreaterThanOrEqual(1)
    await waitUntil(() => current.page.getByText('event sourcing', { exact: false }).count(), count => count > 0, 10_000)
    await assertGolden('[class*="centerCol"]', 'retry.expected.md')
    current.assertClean()
  }, 120_000)

  test('keeps the fixture inventory closed', async () => {
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
      'cancel.expected.md',
      'error-auth.expected.md',
      'loading.expected.md',
      'retry.expected.md',
      'session.jsonl',
    ])
  })
})
