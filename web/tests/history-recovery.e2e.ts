import { mkdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { acknowledgeReloadConnectionLoss, RustWebHarness, settledRecording, waitUntil } from './support'

const PNG_BASE64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII='
const SESSION_COUNT = 701
const SELECTED_ID = 'history-recovery-0000'
const SELECTED_TITLE = 'Recovered old session'
const HISTORY_MARKER = 'RECOVERY_HISTORY_DONE'
const IMAGE_MARKER = 'RECOVERY_IMAGE_DONE'

function recording(index: number): string {
  const suffix = `${index}`.padStart(4, '0')
  const title = index === 0 ? SELECTED_TITLE : `Bulk history ${suffix} ${'x'.repeat(72)}`
  return settledRecording(title, `Archived prompt ${suffix}`, index === 0 ? HISTORY_MARKER : `Archived answer ${suffix}`)
}

function responseStream(): string {
  const message = { id: 'history-image-response', type: 'message', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: IMAGE_MARKER, annotations: [] }] }
  return [
    { type: 'response.created', response: { id: 'history-image-response' } },
    { type: 'response.output_item.added', output_index: 0, item: { ...message, content: [] } },
    { type: 'response.output_text.delta', output_index: 0, delta: IMAGE_MARKER },
    { type: 'response.output_item.done', output_index: 0, item: message },
    { type: 'response.completed', response: { id: 'history-image-response', status: 'completed', output: [message], usage: { input_tokens: 1, output_tokens: 1 } } },
  ].map(event => `data: ${JSON.stringify(event)}\n\n`).join('')
}

test('paginated refresh restores selected old history and a persisted uploaded image', async () => {
  const provider = Bun.serve({ hostname: '127.0.0.1', port: 0, fetch: () => new Response(responseStream(), { headers: { 'content-type': 'text/event-stream' } }) })
  const harness = await RustWebHarness.launch({
    name: 'history-recovery-web-e2e', locale: 'en-US',
    env: { HISTORY_IMAGE_KEY: 'fixture-image-key', DEEPSEEK_API_KEY: '' },
    beforeStart: async candidate => {
      await mkdir(candidate.dataDir, { recursive: true })
      await writeFile(join(candidate.dataDir, 'settings.yaml'), [
        'llm-pi-ai:', '  providers:', '    history-image:', '      displayName: History Image',
        `      baseURL: http://127.0.0.1:${provider.port}/v1`, '      api: openai-responses',
        '      auth: api-key', '      apiKeyEnv: HISTORY_IMAGE_KEY', '      models:',
        '        - id: vision-model', '          name: Vision Model', '          input: [text, image]', '',
      ].join('\n'))
      for (let index = 0; index < SESSION_COUNT; index++) await candidate.seedSession(`history-recovery-${`${index}`.padStart(4, '0')}`, recording(index))
    },
  })
  try {
    const summaries = await harness.sessions()
    expect(summaries.filter(item => item.sessionId.startsWith('history-recovery-')).map(item => item.sessionId))
      .toEqual(Array.from({ length: SESSION_COUNT }, (_, index) => `history-recovery-${`${index}`.padStart(4, '0')}`))
    expect(summaries.map(item => item.sessionId)).toEqual([...summaries].map(item => item.sessionId).sort())
    expect(summaries.find(item => item.sessionId === SELECTED_ID)?.projections?.values).toEqual({ title: SELECTED_TITLE })

    await harness.page.evaluate(sessionId => { localStorage.setItem('dsh.sessions.current', JSON.stringify({ sessionId })) }, SELECTED_ID)
    let warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'domcontentloaded' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await harness.page.getByText(HISTORY_MARKER, { exact: true }).waitFor({ timeout: 30_000 })

    await harness.page.locator('input[type="file"]').setInputFiles({ name: 'persisted.png', mimeType: 'image/png', buffer: Buffer.from(PNG_BASE64, 'base64') })
    await harness.page.locator('textarea').first().fill('Persist this image')
    await harness.page.getByRole('button', { name: 'Select model', exact: true }).click()
    await harness.page.getByRole('menuitem', { name: /Model/ }).click()
    await harness.page.getByRole('menuitemradio', { name: 'Vision Model' }).click()
    await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
    await harness.page.getByText(IMAGE_MARKER, { exact: true }).waitFor({ timeout: 30_000 })

    warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'domcontentloaded' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await harness.page.getByText(HISTORY_MARKER, { exact: true }).waitFor({ timeout: 30_000 })
    await harness.page.getByText(IMAGE_MARKER, { exact: true }).waitFor({ timeout: 30_000 })
    const restored = harness.page.locator('img[alt="persisted.png"]')
    await waitUntil(() => restored.count(), count => count > 0, 30_000)
    await waitUntil(() => restored.first().evaluate(element => {
      const image = element as HTMLImageElement
      return image.complete && image.naturalWidth > 0
    }), ready => ready, 30_000)
    expect(await harness.page.evaluate(() => localStorage.getItem('dsh.sessions.current'))).toContain(SELECTED_ID)
    harness.assertClean()
  } finally {
    await harness.close()
    provider.stop(true)
  }
}, 300_000)
