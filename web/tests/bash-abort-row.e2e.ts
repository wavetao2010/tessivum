import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { openSessionByMarker, RustWebHarness, UPSTREAM_ROOT, waitUntil } from './support'

const SESSION_ID = 'bash-abort-row-web-e2e'
const PROMPT = 'Run two shell commands: wait for cancellation, then write skipped.txt.'
const FIXTURE = join(UPSTREAM_ROOT, 'examples/acp-agent/tests/snapshots/cancel-tool-calls/session.jsonl')

test('bash-abort-row expands an aborted call without terminal result material', async () => {
  const raw = await readFile(FIXTURE, 'utf8')
  expect(raw).toContain(PROMPT)
  const harness = await RustWebHarness.launch({
    name: 'bash-abort-row-web-e2e',
    beforeStart: candidate => candidate.seedSession(SESSION_ID, raw),
  })
  try {
    await openSessionByMarker(harness, PROMPT)
    const row = harness.page.locator('[data-sample="bash"]').first()
    const call = row.locator('xpath=..')
    await waitUntil(() => row.getAttribute('aria-expanded'), value => value === 'false')
    expect(await call.getByText('Error: tool call aborted', { exact: true }).count()).toBe(1)
    await row.click()

    await waitUntil(() => row.getAttribute('aria-expanded'), value => value === 'true')
    await call.getByText('IN', { exact: true }).waitFor()
    await call.getByText('OUT', { exact: true }).waitFor()
    await call.getByText('Wait until cancellation', { exact: false }).waitFor()
    await call.getByText('setInterval(() => {}, 1000)', { exact: false }).waitFor()
    expect(await call.getByText('Error: tool call aborted', { exact: true }).count()).toBe(2)
    const skipped = harness.page.locator('[data-chat-call-id="call_skipped"]')
    expect(await skipped.getByText('Error: tool call aborted before dispatch', { exact: true }).count()).toBe(1)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
