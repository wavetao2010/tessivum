import { readFile } from 'node:fs/promises'
import { afterAll, beforeAll, describe, expect, test } from 'bun:test'
import { acknowledgeReloadConnectionLoss, fixture, materializeRecording, RustWebHarness, waitUntil } from './support'

const SEED_ID = 'message-feedback-web-e2e'
const NOTE = 'Read both files before answering.'

describe('durable per-message feedback', () => {
  let harness: RustWebHarness

  beforeAll(async () => {
    harness = await RustWebHarness.launch({
      name: 'message-feedback',
      beforeStart: async candidate => {
        const seed = materializeRecording((await readFile(await fixture('seeded-history', 'seed.jsonl'), 'utf8'))
          .replaceAll('{{cwd}}/workspace', '{{cwd}}'))
        await candidate.seedSession(SEED_ID, seed)
      },
    })
  }, 120_000)

  afterAll(async () => {
    await harness?.close()
  })

  async function openSeededSession(): Promise<void> {
    if (await harness.page.getByText('DONE', { exact: true }).count() > 0) return
    const workspace = harness.page.getByRole('treeitem', { name: 'workspace', exact: true })
    if (await workspace.getAttribute('aria-expanded') !== 'true') await workspace.click()
    await harness.page.getByRole('treeitem', { name: /Use the read tool twice/ }).last().click()
  }

  test('persists a rating and its note across a reload, then retracts', async () => {
    await openSeededSession()
    await harness.page.getByText('DONE', { exact: true }).waitFor({ timeout: 30_000 })
    const like = harness.page.getByRole('button', { name: 'Good response' }).first()
    await like.waitFor({ timeout: 30_000 })
    await like.scrollIntoViewIfNeeded()
    await like.hover()
    await like.click()
    const rated = harness.page.getByRole('button', { name: 'Remove rating' }).first()
    await waitUntil(() => rated.getAttribute('aria-pressed'), pressed => pressed === 'true', 10_000)

    await harness.page.getByRole('button', { name: 'Add a note' }).first().click()
    const editor = harness.page.getByRole('textbox', { name: 'Feedback note' })
    await editor.fill(NOTE)
    await harness.page.getByRole('button', { name: 'Save', exact: true }).click()
    await waitUntil(() => editor.count(), count => count === 0, 10_000)
    await harness.page.getByText(NOTE, { exact: true }).waitFor({ timeout: 10_000 })

    const warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    await openSeededSession()
    await harness.page.getByText('DONE', { exact: true }).waitFor({ timeout: 30_000 })

    const cold = harness.page.getByRole('button', { name: 'Good response' }).first()
    await cold.waitFor({ timeout: 30_000 })
    await cold.scrollIntoViewIfNeeded()
    await cold.hover()

    const restored = harness.page.getByRole('button', { name: 'Remove rating' }).first()
    await restored.waitFor({ timeout: 30_000 })
    await restored.scrollIntoViewIfNeeded()
    await restored.hover()
    await waitUntil(() => restored.getAttribute('aria-pressed'), pressed => pressed === 'true', 15_000)
    await harness.page.getByText(NOTE, { exact: true }).waitFor({ timeout: 10_000 })

    await restored.click()
    await waitUntil(
      () => harness.page.getByRole('button', { name: 'Good response' }).first().getAttribute('aria-pressed'),
      pressed => pressed === 'false',
      10_000,
    )
    await waitUntil(() => harness.page.getByText(NOTE, { exact: true }).count(), count => count === 0, 10_000)
  }, 90_000)

  test('keeps the console clean', () => {
    harness.assertClean()
  })
})
