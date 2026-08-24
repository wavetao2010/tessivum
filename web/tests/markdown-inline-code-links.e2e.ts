import { afterAll, beforeAll, expect, test } from 'bun:test'
import { join } from 'node:path'
import { openSeededSession, RustWebHarness, settledRecording, stableAria } from './support'

const SEED_ID = 'markdown-inline-code-links-web-e2e'
const DONE = 'INLINE_CODE_LINK_DONE'
const GOLDEN = join(import.meta.dir, 'snapshots/markdown-inline-code-links/ui.expected.yml')
let harness: RustWebHarness
let linkUrl: string

beforeAll(async () => {
  harness = await RustWebHarness.launch({
    name: 'markdown-inline-code-links',
    beforeStart: async candidate => {
      linkUrl = `${candidate.baseUrl}/?demo=1`
      const markdown = [
        '## Inline code links',
        '',
        `Preview: \`${linkUrl}\``,
        '',
        `Standard: [Open preview](${linkUrl})`,
        '',
        `Command: \`curl ${linkUrl}\``,
        '',
        'Unsafe: `javascript:alert(1)`',
        '',
        DONE,
      ].join('\n')
      await candidate.seedSession(SEED_ID, settledRecording('Inline code links', 'Show the local preview URL.', markdown))
    },
  })
}, 120_000)

afterAll(async () => {
  await harness?.close()
})

test('opens complete HTTP inline code links and leaves other code inert', async () => {
  await openSeededSession(harness, DONE)
  const inlineCodeLink = harness.page.locator('[class*="markdown"] code a')
  await inlineCodeLink.waitFor({ timeout: 10_000 })
  expect(await inlineCodeLink.count()).toBe(1)
  expect(await inlineCodeLink.getAttribute('href')).toBe(linkUrl)
  expect(await inlineCodeLink.getAttribute('target')).toBe('_blank')
  expect(await inlineCodeLink.getAttribute('rel')).toBe('noopener noreferrer')
  await inlineCodeLink.focus()
  expect(await inlineCodeLink.evaluate(element => document.activeElement === element)).toBe(true)

  const popupPromise = harness.page.waitForEvent('popup')
  await inlineCodeLink.click()
  const popup = await popupPromise
  await popup.waitForURL(linkUrl, { timeout: 15_000 })
  await popup.close()

  expect(await harness.page.getByText(`curl ${linkUrl}`, { exact: true }).locator('a').count()).toBe(0)
  expect(await harness.page.getByText('javascript:alert(1)', { exact: true }).locator('a').count()).toBe(0)
  const snapshot = stableAria(await harness.page.locator('[class*="centerCol"]').ariaSnapshot()).split(linkUrl).join('{{linkUrl}}')
  expect(snapshot).toBe((await Bun.file(GOLDEN).text()).trim())
  harness.assertClean()
}, 60_000)
