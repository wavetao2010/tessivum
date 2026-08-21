import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, openSeededSession, RustWebHarness, stableAria, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/stats-paged-history')
const UI_EXPECTED = join(SNAPSHOT_DIR, 'ui.expected.md')
const SEED_ID = 'stats-paged-history-web-e2e'
const TURNS = 28
const FULL_COUNTS = `${TURNS} turns · ${TURNS} steps`

function buildSeed(turns: number): string {
  const lines = [JSON.stringify({
    type: 'session', version: 0, id: '{{sessionId}}', createdAt: 1784974100000, cwd: '{{cwd}}/workspace',
  })]
  let seq = 0
  let time = 1784974100000
  const at = (event: Record<string, unknown>): void => { lines.push(JSON.stringify({ ...event, seq: seq++, time: time++ })) }
  for (let turn = 1; turn <= turns; turn += 1) {
    at({ type: 'turn/start', data: { turn } })
    at({ type: 'user/message', data: { content: [{ type: 'text', text: `m${turn}` }], source: { kind: 'user' } }, surfaceOp: 'append' })
    at({ type: 'step/start', data: { turn, step: 1 } })
    at({
      type: 'assistant/message',
      data: {
        turn,
        step: 1,
        message: {
          id: `00000000-0000-4000-8000-${String(turn).padStart(12, '0')}`,
          role: 'assistant',
          content: [{ type: 'text', text: `r${turn}` }],
          source: { kind: 'model', provider: 'snapshot', model: 'snapshot-replier' },
        },
      },
      sourceEventSeqs: [],
      surfaceOp: 'append',
    })
    at({ type: 'step/end', data: { turn, step: 1 } })
    at({ type: 'turn/end', data: { turn, reason: { kind: 'completed' } } })
  }
  return `${lines.join('\n')}\n`
}

async function captureStatsAria(harness: RustWebHarness): Promise<string> {
  const base = harness.workspace.split('/').at(-1) ?? harness.workspace
  return stableAria(await captureStableAria(harness.page, '[class*="centerCol"]'))
    .split(harness.workspace).join('{{cwd}}')
    .split(base).join('{{workspace}}')
}

test('stats-paged-history keeps whole-session counts while native history prepends', async () => {
  const harness = await RustWebHarness.launch({
    name: 'stats-paged-history-web-e2e',
    locale: 'en-US',
    beforeStart: candidate => candidate.seedSession(SEED_ID, buildSeed(TURNS)),
  })
  try {
    await openSeededSession(harness, `r${TURNS}`)
    expect(await harness.page.getByText('m1', { exact: true }).count()).toBe(0)
    await waitUntil(() => harness.page.getByText(FULL_COUNTS, { exact: false }).count(), count => count === 1, 10_000)
    const strip = harness.page.getByText(FULL_COUNTS, { exact: false }).locator('..')
    const before = await strip.textContent()

    await harness.page.getByRole('button', { name: 'Load earlier', exact: true }).click()
    await harness.page.getByText('m1', { exact: true }).waitFor({ timeout: 10_000 })
    expect(await strip.textContent()).toBe(before)
    expect(await harness.page.locator('[data-chat-flow-key^="9:turn-tail"]').count()).toBe(TURNS)
    expect(`${await captureStatsAria(harness)}\n`).toBe(await readFile(UI_EXPECTED, 'utf8'))
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 60_000)

test('stats-paged-history fixture inventory remains closed', async () => {
  expect(await readdir(SNAPSHOT_DIR)).toEqual(['ui.expected.md'])
})
