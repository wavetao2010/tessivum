import { mkdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness } from './support'

const SESSION_ID = 'cold-blank-session-web-e2e'
const WORKSPACE_NAME = 'cold-blank-workspace'

test('keeps a materialized cold blank session out of the sidebar', async () => {
  const harness = await RustWebHarness.launch({ name: 'cold-blank-session-web-e2e' })
  const cwd = join(harness.workspace, WORKSPACE_NAME)
  await mkdir(cwd)
  const time = 1_785_000_000_000
  await harness.seedSession(SESSION_ID, [
    JSON.stringify({ type: 'session', version: 0, id: SESSION_ID, createdAt: time, cwd }),
    JSON.stringify({ type: 'session/end-seed', time, seq: 0, data: {} }),
  ].join('\n'))
  await harness.page.reload({ waitUntil: 'load' })
  try {
    const session = (await harness.sessions()).find(candidate => candidate.sessionId === SESSION_ID)
    expect(session).toMatchObject({ blank: true })
    const tree = harness.page.getByRole('tree', { name: 'Sessions' })
    await tree.waitFor({ timeout: 15_000 })
    expect(await tree.getByText(WORKSPACE_NAME, { exact: true }).count()).toBe(0)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 60_000)
