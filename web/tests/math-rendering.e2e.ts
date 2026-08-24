import { afterAll, beforeAll, expect, test } from 'bun:test'
import { join } from 'node:path'
import { openSeededSession, RustWebHarness, settledRecording, stableAria } from './support'

const SEED_ID = 'math-rendering-web-e2e'
const DONE = 'MATH_RENDERING_DONE'
const GOLDEN = join(import.meta.dir, 'snapshots/math-rendering/ui.expected.yml')
let harness: RustWebHarness

beforeAll(async () => {
  const markdown = [
    '## Math rendering',
    '',
    'Inline dollar $\\theta$ and backslash \\(\\frac{1}{5}\\).',
    '',
    '\\[\\frac{\\pi}{4} < \\theta < \\frac{\\pi}{2}\\]',
    '',
    '$$\\theta \\in \\left(\\frac{\\pi}{4}, \\frac{\\pi}{2}\\right). \\tag{1}$$',
    '',
    '| Symbol | Value |',
    '| --- | --- |',
    '| $\\theta$ | \\(\\frac{1}{5}\\) |',
    '',
    DONE,
  ].join('\n')
  harness = await RustWebHarness.launch({
    name: 'math-rendering',
    beforeStart: async candidate => candidate.seedSession(SEED_ID, settledRecording('Math rendering', 'Render this mathematical proof.', markdown)),
  })
}, 120_000)

afterAll(async () => {
  await harness?.close()
})

test('renders every supported math delimiter without KaTeX errors', async () => {
  await openSeededSession(harness, DONE)
  expect(await harness.page.locator('.katex').count()).toBe(6)
  expect(await harness.page.locator('.katex-display').count()).toBe(2)
  expect(await harness.page.locator('.katex-error').count()).toBe(0)
  expect(stableAria(await harness.page.locator('[class*="centerCol"]').ariaSnapshot())).toBe((await Bun.file(GOLDEN).text()).trim())
  harness.assertClean()
}, 60_000)
