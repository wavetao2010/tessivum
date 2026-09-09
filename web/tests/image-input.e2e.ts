import { mkdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

const PNG_BASE64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII='
const PNG = Buffer.from(PNG_BASE64, 'base64')
const MARKER = 'VISION_INPUT_ACCEPTED'

function responseStream(): string {
  const message = {
    id: 'msg-vision-input',
    type: 'message',
    role: 'assistant',
    status: 'completed',
    content: [{ type: 'output_text', text: MARKER, annotations: [] }],
  }
  return [
    { type: 'response.created', response: { id: 'resp-vision-input' } },
    { type: 'response.output_item.added', output_index: 0, item: { ...message, content: [] } },
    { type: 'response.output_text.delta', output_index: 0, delta: MARKER },
    { type: 'response.output_item.done', output_index: 0, item: message },
    {
      type: 'response.completed',
      response: {
        id: 'resp-vision-input', status: 'completed', output: [message],
        usage: { input_tokens: 1, output_tokens: 1 },
      },
    },
  ].map(event => `data: ${JSON.stringify(event)}\n\n`).join('')
}

test('image drafts stay model-independent until explicit capability admission', async () => {
  const requests: unknown[] = []
  const provider = Bun.serve({
    hostname: '127.0.0.1',
    port: 0,
    async fetch(request) {
      if (new URL(request.url).pathname !== '/v1/responses') return new Response(null, { status: 404 })
      requests.push(await request.json())
      return new Response(responseStream(), { headers: { 'content-type': 'text/event-stream' } })
    },
  })
  const harness = await RustWebHarness.launch({
    name: 'image-input-web-e2e',
    locale: 'zh-CN',
    env: { IMAGE_ROUTE_KEY: 'fixture-image-key', DEEPSEEK_API_KEY: '' },
    beforeStart: async candidate => {
      await mkdir(candidate.dataDir, { recursive: true })
      await writeFile(join(candidate.dataDir, 'settings.yaml'), [
        'llm-pi-ai:',
        '  providers:',
        '    image-fixture:',
        '      displayName: Image Fixture',
        `      baseURL: http://127.0.0.1:${provider.port}/v1`,
        '      api: openai-responses',
        '      auth: api-key',
        '      apiKeyEnv: IMAGE_ROUTE_KEY',
        '      models:',
        '        - id: text-model',
        '          name: Text Model',
        '          input: [text]',
        '        - id: vision-model',
        '          name: Vision Model',
        '          input: [text, image]',
        '',
      ].join('\n'))
    },
  })
  try {
    const textarea = harness.page.locator('textarea').first()
    await textarea.fill('Describe all three images')
    await harness.page.locator('input[type="file"]').setInputFiles({
      name: 'picked.png', mimeType: 'image/png', buffer: PNG,
    })
    await textarea.evaluate((element, base64) => {
      const bytes = Uint8Array.from(atob(base64), character => character.charCodeAt(0))
      const file = new File([bytes], 'pasted.png', { type: 'image/png' })
      const event = new Event('paste', { bubbles: true, cancelable: true })
      Object.defineProperty(event, 'clipboardData', {
        value: { items: [{ kind: 'file', type: file.type, getAsFile: () => file }], getData: () => '' },
      })
      element.dispatchEvent(event)
    }, PNG_BASE64)
    await harness.page.evaluate((base64) => {
      const bytes = Uint8Array.from(atob(base64), character => character.charCodeAt(0))
      const file = new File([bytes], 'dropped.png', { type: 'image/png' })
      const event = new Event('drop', { bubbles: true, cancelable: true })
      Object.defineProperty(event, 'dataTransfer', {
        value: { types: ['Files'], files: [file], dropEffect: 'none' },
      })
      document.body.dispatchEvent(event)
    }, PNG_BASE64)

    const rail = harness.page.getByRole('group', { name: '待发送图片' })
    await waitUntil(() => rail.locator('img').count(), count => count === 3)
    expect(await rail.locator('img').evaluateAll(images => images.map(image => image.getAttribute('alt'))))
      .toEqual(['picked.png', 'pasted.png', 'dropped.png'])
    expect(await textarea.inputValue()).toBe('Describe all three images')

    const trigger = harness.page.getByRole('button', { name: '选择模型', exact: true })
    await trigger.click()
    await harness.page.getByRole('menuitem', { name: /模型/ }).click()
    const textModel = harness.page.getByRole('menuitemradio', { name: 'Text Model' })
    expect(await textModel.textContent()).toContain('仅支持文本')
    await textModel.click()
    await harness.page.getByRole('button', { name: '发送消息', exact: true }).click()
    await harness.page.getByText('当前模型不支持图片，请切换支持图片的模型', { exact: true }).waitFor({ timeout: 10_000 })
    expect(requests).toEqual([])
    expect(await textarea.inputValue()).toBe('Describe all three images')
    expect(await rail.locator('img').count()).toBe(3)

    await harness.page.getByRole('button', { name: /^选择模型，当前 Text Model/ }).click()
    await harness.page.getByRole('menuitem', { name: /模型/ }).click()
    const visionModel = harness.page.getByRole('menuitemradio', { name: 'Vision Model' })
    expect(await visionModel.textContent()).toContain('支持图片')
    await visionModel.click()
    await harness.page.getByRole('button', { name: '发送消息', exact: true }).click()
    await harness.page.getByText(MARKER, { exact: true }).waitFor({ timeout: 30_000 })

    expect(requests).toHaveLength(1)
    expect(JSON.stringify(requests[0]).split(`data:image/png;base64,${PNG_BASE64}`).length - 1).toBe(3)
    expect(await textarea.inputValue()).toBe('')
    expect(await harness.page.getByText('当前模型不支持图片，请切换支持图片的模型', { exact: true }).count()).toBe(0)
    expect(await rail.count()).toBe(0)
    harness.assertClean()
  } finally {
    await harness.close()
    provider.stop(true)
  }
}, 120_000)
