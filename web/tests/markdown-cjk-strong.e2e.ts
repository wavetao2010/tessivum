import { afterAll, beforeAll, expect, test } from 'bun:test'
import { openSeededSession, RustWebHarness, settledRecording } from './support'

const SEED_ID = 'markdown-cjk-strong-web-e2e'
const DONE = 'CJK_STRONG_DONE'
const CASES = [
  ['**注意：**内容', '注意：', '注意：内容'],
  ['**Notice:**内容', 'Notice:', 'Notice:内容'],
  ['**事件中间件（waterfall）**实现', '事件中间件（waterfall）', '事件中间件（waterfall）实现'],
  ['**事件中间件(waterfall)**实现', '事件中间件(waterfall)', '事件中间件(waterfall)实现'],
  ['**句号。**后续', '句号。', '句号。后续'],
  ['**Period.**后续', 'Period.', 'Period.后续'],
  ['**提醒！**继续', '提醒！', '提醒！继续'],
  ['**Warning!**继续', 'Warning!', 'Warning!继续'],
] as const

let harness: RustWebHarness

function recording(): string {
  const text = ['## CJK strong emphasis', '', ...CASES.flatMap(([markdown]) => [markdown, '']), DONE].join('\n')
  return settledRecording('CJK strong emphasis', 'Render adjacent CJK strong emphasis.', text)
}


beforeAll(async () => {
  harness = await RustWebHarness.launch({
    name: 'markdown-cjk-strong',
    beforeStart: async candidate => candidate.seedSession(SEED_ID, recording()),
  })
}, 120_000)

afterAll(async () => {
  await harness?.close()
})

test('renders punctuation-terminated strong spans before adjacent CJK text', async () => {
  await openSeededSession(harness, DONE)
  const strong = harness.page.locator('[class*="markdown"] strong')
  expect(await strong.count()).toBe(CASES.length)
  expect(await strong.allTextContents()).toEqual(CASES.map(([, expected]) => expected))
  for (const [, , paragraph] of CASES) {
    expect(await harness.page.getByText(paragraph, { exact: true }).count()).toBe(1)
  }
  harness.assertClean()
}, 60_000)
