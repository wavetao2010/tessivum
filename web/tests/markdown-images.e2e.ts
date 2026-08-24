import { afterAll, beforeAll, expect, test } from 'bun:test'
import { createServer, type Server } from 'node:http'
import { join } from 'node:path'
import { openSeededSession, RustWebHarness, settledRecording, stableAria, waitUntil } from './support'

const SEED_ID = 'markdown-images-web-e2e'
const DONE = 'REMOTE_IMAGE_DONE'
const REMOTE_ALT = 'Remote test image'
const LOCAL_ALT = 'Local test image'
const GOLDEN = join(import.meta.dir, 'snapshots/markdown-images/ui.expected.yml')
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=', 'base64')
let harness: RustWebHarness
let imageServer: Server
let imageUrl: string
const requests: Array<{ path: string | undefined; referer: string | undefined }> = []

beforeAll(async () => {
  imageServer = createServer((request, response) => {
    requests.push({ path: request.url, referer: request.headers.referer })
    response.writeHead(200, { 'cache-control': 'no-store', 'content-length': PNG.length, 'content-type': 'image/png' })
    response.end(PNG)
  })
  await new Promise<void>((resolve, reject) => {
    imageServer.once('error', reject)
    imageServer.listen(0, '127.0.0.1', resolve)
  })
  const address = imageServer.address()
  if (address === null || typeof address === 'string') throw new Error('image origin did not expose an IP socket')
  imageUrl = `http://127.0.0.1:${address.port}/image.png`
  const markdown = ['## Markdown images', '', `![${REMOTE_ALT}](${imageUrl})`, '', `![${LOCAL_ALT}](./local-image.png)`, '', DONE].join('\n')
  harness = await RustWebHarness.launch({
    name: 'markdown-images',
    beforeStart: async candidate => candidate.seedSession(SEED_ID, settledRecording('Markdown image policy', 'Show the Markdown image policy.', markdown)),
  })
}, 120_000)

afterAll(async () => {
  await harness?.close()
  if (imageServer !== undefined) await new Promise<void>((resolve, reject) => imageServer.close(error => error ? reject(error) : resolve()))
})

test('loads only absolute HTTP Markdown images without a referrer', async () => {
  await openSeededSession(harness, DONE)
  const image = harness.page.getByRole('img', { name: REMOTE_ALT })
  await image.waitFor({ timeout: 10_000 })
  await waitUntil(() => image.evaluate(element => (element as HTMLImageElement).naturalWidth), width => width > 0)
  expect(await image.evaluate(element => {
    const computed = getComputedStyle(element)
    return {
      borderRadius: computed.borderRadius,
      decoding: element.getAttribute('decoding'),
      loading: element.getAttribute('loading'),
      maxWidth: computed.maxWidth,
      referrerPolicy: element.getAttribute('referrerpolicy'),
    }
  })).toEqual({ borderRadius: '8px', decoding: 'async', loading: 'lazy', maxWidth: '100%', referrerPolicy: 'no-referrer' })
  expect(await harness.page.getByRole('img', { name: LOCAL_ALT }).count()).toBe(0)
  expect(await harness.page.getByText(LOCAL_ALT, { exact: true }).count()).toBe(1)
  expect(requests).toEqual([{ path: '/image.png', referer: undefined }])
  expect(stableAria(await harness.page.locator('[class*="centerCol"]').ariaSnapshot())).toBe((await Bun.file(GOLDEN).text()).trim())
  harness.assertClean()
}, 60_000)
