import { expect, test } from 'bun:test'
import { join } from 'node:path'
import { acknowledgeReloadConnectionLoss, captureStableAria, RustWebHarness, UPSTREAM_ROOT, waitUntil } from './support'

const FIXTURE = join(UPSTREAM_ROOT, 'examples/acp-agent/tests/snapshots/workflow-run/session.jsonl')
const UI_EXPECTED = `${import.meta.dir}/snapshots/workflow-run/ui.expected.md`
const CHILD_PROMPT = 'Reply with exactly the word WF_CHILD_OK and nothing else.'
const PROMPT = `Use the workflow tool exactly once, with args omitted, meta set to { "name": "snapshot-flow", "description": "one child for the snapshot" }, and this EXACT script body (copy it verbatim):
phase('Run')
const reply = await agent('Reply with exactly the word WF_CHILD_OK and nothing else.')
return { reply }
After the workflow returns, reply with the single word WORKFLOW_DONE and stop. Do not use any other tool.`

test('workflow run exposes its child, settles beside the tool row, and rebuilds after reload', async () => {
  const harness = await RustWebHarness.launch({ name: 'workflow-run', locale: 'en-US', replayFixture: FIXTURE })
  try {
    const input = harness.page.locator('textarea:enabled').first()
    await input.fill(PROMPT)
    await input.press('Enter')

    const workflow = harness.page.locator('[data-workflow-run]')
    await workflow.waitFor({ timeout: 60_000 })
    if (await workflow.getAttribute('data-run-status') !== 'running') {
      await workflow.getByRole('button', { name: /^snapshot-flow / }).click()
    }
    const disclosures = workflow.locator('[data-disclosure-row][role="button"]')
    await disclosures.nth(1).waitFor({ timeout: 15_000 })
    for (const disclosure of [disclosures.nth(0), disclosures.nth(1)]) {
      expect(await disclosure.getAttribute('role')).toBe('button')
      if (await disclosure.getAttribute('aria-expanded') === 'false') await disclosure.click()
      expect(await disclosure.getAttribute('aria-expanded')).toBe('true')
    }
    const sessionId = (await harness.sessions()).find(item => !item.blank)?.sessionId
    if (sessionId === undefined) throw new Error('workflow created no nonblank session')
    await waitUntil(async () => {
      const history = await harness.rpc<{ events: Array<{ event: { type: string } }> }>(
        'session.history',
        { sessionId, maxMessages: 1_000 },
      )
      return history.value?.events.some(entry => entry.event.type === 'turn/end') ?? false
    }, Boolean, 60_000)

    const terminal = harness.page.locator('[data-workflow-run][data-run-status="completed"]')
    await terminal.waitFor({ timeout: 15_000 })
    expect(await harness.page.locator('[data-chat-flow-kind="tool-call"]').count()).toBeGreaterThanOrEqual(1)
    expect(await harness.page.locator('[data-chat-flow-kind="workflow-run"]').count()).toBe(1)
    const record = harness.page.getByRole('button', { name: /^snapshot-flow/ })
    await record.waitFor({ timeout: 15_000 })
    if (await record.getAttribute('aria-expanded') === 'true') await record.click()
    expect(await record.getAttribute('aria-expanded')).toBe('false')
    expect(await record.evaluate(element => getComputedStyle(element).cursor)).toBe('pointer')
    await record.click()
    const phase = harness.page.getByRole('button', { name: /^Run/ })
    await phase.waitFor({ timeout: 15_000 })
    expect(await phase.getAttribute('aria-expanded')).toBe('false')
    expect(await phase.evaluate(element => getComputedStyle(element).cursor)).toBe('pointer')
    await phase.click()
    await harness.page.getByText(CHILD_PROMPT, { exact: false }).waitFor({ timeout: 15_000 })
    expect(await harness.page.getByRole('button', { name: /^Open Reply with exactly the word/ }).count()).toBe(0)

    const warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    const replayed = harness.page.getByRole('button', { name: /^snapshot-flow/ })
    await replayed.waitFor({ timeout: 15_000 })
    expect(await replayed.getAttribute('aria-expanded')).toBe('false')
    await replayed.click()
    const replayedPhase = harness.page.getByRole('button', { name: /^Run/ })
    await replayedPhase.waitFor({ timeout: 15_000 })
    expect(await replayedPhase.getAttribute('aria-expanded')).toBe('false')
    await replayedPhase.click()
    await harness.page.getByText(CHILD_PROMPT, { exact: false }).waitFor({ timeout: 15_000 })
    expect(await harness.page.getByRole('button', { name: /^Open Reply with exactly the word/ }).count()).toBe(0)
    expect(await captureStableAria(harness.page, '[data-chat-flow]')).toBe((await Bun.file(UI_EXPECTED).text()).trim())
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
