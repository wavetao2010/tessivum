import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, openSessionByMarker, RustWebHarness, settledRecording } from './support'

const SESSION_ID = 'background-job-list-web-e2e'
const SEED = 'BACKGROUND_JOB_LIST_SEED'
const DONE = 'BACKGROUND_JOB_LIST_DONE'
const CALL_ID = 'background-job-list-call'
const CANCEL_ID = 'background-job-list-cancel'
const CANCELLED = 'BACKGROUND_JOB_LIST_CANCELLED'
const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/background-job-list')
const RUNNING_EXPECTED = join(SNAPSHOT_DIR, 'running.expected.md')
const SETTLED_EXPECTED = join(SNAPSHOT_DIR, 'settled.expected.md')
const COMMAND = 'sleep 45'

function replayRecording(): string {
  const argumentsJson = JSON.stringify({ command: COMMAND, description: 'Hold a background slot open', run_in_background: true })
  const attempts = [
    [
      { type: 'block-start', index: 0, blockType: 'tool-call' },
      { type: 'block-end', index: 0, block: { type: 'tool-call', id: CALL_ID, name: 'bash', arguments: argumentsJson } },
      { type: 'finish', reason: { kind: 'tool-calls' } },
    ],
    [
      { type: 'block-start', index: 0, blockType: 'text' },
      { type: 'block-end', index: 0, block: { type: 'text', text: DONE } },
      { type: 'finish', reason: { kind: 'stop' } },
    ],
    [
      { type: 'block-start', index: 0, blockType: 'tool-call' },
      { type: 'block-end', index: 0, block: { type: 'tool-call', id: CANCEL_ID, name: 'jobs.kill', arguments: JSON.stringify({ jobId: 'bash-1' }) } },
      { type: 'finish', reason: { kind: 'tool-calls' } },
    ],
    [
      { type: 'block-start', index: 0, blockType: 'text' },
      { type: 'block-end', index: 0, block: { type: 'text', text: CANCELLED } },
      { type: 'finish', reason: { kind: 'stop' } },
    ],
  ]
  let seq = 0
  return [
    { type: 'session', version: 0, id: 'background-job-list-replay', createdAt: 0, cwd: '/workspace' },
    ...attempts.flatMap((chunks, attempt) => chunks.map(chunk => ({
      type: 'assistant/chunk', seq: seq++, time: 0, data: { turn: attempt + 1, step: 1, chunk },
    }))),
  ].map(row => JSON.stringify(row)).join('\n')
}

test('background-job-list streams running and settled jobs into the session header', async () => {
  const harness = await RustWebHarness.launch({
    name: 'background-job-list-web-e2e',
    locale: 'en-US',
    replayRecording: replayRecording(),
    beforeStart: candidate => candidate.seedSession(
      SESSION_ID,
      settledRecording('BACKGROUND_JOB_LIST fixture', SEED, 'BACKGROUND_JOB_LIST_READY'),
    ),
  })
  try {
    await openSessionByMarker(harness, SEED, 'BACKGROUND_JOB_LIST_READY')
    expect(await harness.page.getByRole('button', { name: '1 background job running' }).count()).toBe(0)

    const settled = harness.whenTurnSettled()
    await harness.page.locator('textarea:enabled').last().fill('Start the background diagnostic.')
    await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
    await settled
    await harness.page.getByText(DONE, { exact: true }).waitFor({ timeout: 15_000 })

    const running = harness.page.getByRole('button', { name: '1 background job running' })
    await running.waitFor({ timeout: 15_000 })
    await running.click()
    const row = harness.page.getByRole('list', { name: 'Background jobs' }).getByRole('listitem').first()
    await row.waitFor()
    await row.getByText(COMMAND, { exact: true }).waitFor()
    await row.getByText('running', { exact: true }).waitFor()
    expect(await captureStableAria(harness.page, '[class*="menu"]')).toBe((await readFile(RUNNING_EXPECTED, 'utf8')).trim())

    const cancelled = harness.whenTurnSettled()
    const prompted = await harness.rpc<{ accepted: boolean }>('session.prompt', {
      sessionId: SESSION_ID,
      mode: 'queue',
      content: [{ type: 'text', text: 'Cancel the background diagnostic.' }],
    })
    expect(prompted).toMatchObject({ ok: true, value: { accepted: true } })
    await cancelled
    await harness.page.getByText(CANCELLED, { exact: true }).waitFor({ timeout: 15_000 })
    const idle = harness.page.getByRole('button', { name: '1 background job', exact: true })
    await idle.waitFor({ timeout: 20_000 })
    if (await idle.getAttribute('aria-expanded') !== 'true') await idle.click()
    expect(await captureStableAria(harness.page, '[class*="menu"]')).toBe((await readFile(SETTLED_EXPECTED, 'utf8')).trim())
    await row.getByText('signal: SIGTERM', { exact: true }).waitFor({ timeout: 20_000 })
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['running.expected.md', 'settled.expected.md'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
