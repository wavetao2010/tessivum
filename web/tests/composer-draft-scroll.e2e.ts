import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

const FIRST_MARKER = 'FIRST-LINE-MARKER'
const LAST_MARKER = 'LAST-LINE-MARKER'
const DRAFT_LINES = 40
const DRAFT = Array.from({ length: DRAFT_LINES }, (_, index) => {
  if (index === 0) return FIRST_MARKER
  if (index === DRAFT_LINES - 1) return LAST_MARKER
  return `draft line ${String(index + 1).padStart(2, '0')}`
}).join('\n')

interface ComposerMetrics {
  backdropWrapWidth: number
  caretGlyphGap: number
  clientHeight: number
  firstLineOffset: number
  gapShiftOnScroll: number
  inputScrollable: number
  inputWrapWidth: number
  lastLineOffset: number
  mirrorWrapWidth: number
  overflows: boolean
  scrollMax: number
  scrollTop: number
  visibleLines: number
}

function measureComposer(harness: RustWebHarness): Promise<ComposerMetrics> {
  return harness.page.evaluate(({ first, last }) => {
    const input = document.querySelector<HTMLTextAreaElement>('textarea:enabled')
    if (input === null) throw new Error('no live composer textarea in the DOM')
    const scroll = input.closest<HTMLElement>('[data-input-scroll]')
    const backdrop = input.parentElement?.querySelector<HTMLElement>('[data-input-backdrop]')
    const mirror = input.nextElementSibling
    if (scroll === null || backdrop === null || !(mirror instanceof HTMLElement)) throw new Error('composer text layers are incomplete')
    const text = backdrop.firstChild
    if (!(text instanceof Text)) throw new Error('backdrop does not start with the draft text')
    const glyphTop = (marker: string): number => {
      const offset = text.data.indexOf(marker)
      if (offset < 0) throw new Error(`backdrop marker ${marker} is missing`)
      const range = document.createRange()
      range.setStart(text, offset)
      range.setEnd(text, offset + marker.length)
      return range.getBoundingClientRect().top
    }
    const paddingTop = Number.parseFloat(getComputedStyle(input).paddingTop)
    const gap = (): number => Math.round(input.getBoundingClientRect().top + paddingTop - input.scrollTop - glyphTop(first))
    const before = gap()
    const restore = scroll.scrollTop
    scroll.scrollTop = restore === 0 ? 120 : 0
    const gapShiftOnScroll = Math.abs(gap() - before)
    scroll.scrollTop = restore
    const box = scroll.getBoundingClientRect()
    return {
      backdropWrapWidth: backdrop.clientWidth,
      caretGlyphGap: before,
      clientHeight: scroll.clientHeight,
      firstLineOffset: glyphTop(first) - box.top,
      gapShiftOnScroll,
      inputScrollable: input.scrollHeight - input.clientHeight,
      inputWrapWidth: input.clientWidth,
      lastLineOffset: glyphTop(last) - box.top,
      mirrorWrapWidth: mirror.clientWidth,
      overflows: scroll.scrollHeight > scroll.clientHeight,
      scrollMax: scroll.scrollHeight - scroll.clientHeight,
      scrollTop: scroll.scrollTop,
      visibleLines: Math.floor(scroll.clientHeight / Number.parseFloat(getComputedStyle(input).lineHeight)),
    }
  }, { first: FIRST_MARKER, last: LAST_MARKER })
}

async function wheelDraft(harness: RustWebHarness, deltaY: number): Promise<void> {
  const input = harness.page.locator('textarea:enabled').first()
  await input.hover()
  await harness.page.mouse.wheel(0, deltaY)
}

test('composer-draft-scroll', async () => {
  const harness = await RustWebHarness.launch({ name: 'composer-draft-scroll-web-e2e' })
  try {
    const input = harness.page.locator('textarea:enabled').first()
    await input.fill(DRAFT)
    await waitUntil(() => measureComposer(harness), metrics => metrics.overflows)

    await wheelDraft(harness, -2_000)
    await waitUntil(() => measureComposer(harness), metrics => metrics.scrollTop === 0)
    const top = await measureComposer(harness)
    expect(top.visibleLines).toBe(14)
    expect(top.inputScrollable).toBe(0)
    expect(top.inputWrapWidth).toBe(top.backdropWrapWidth)
    expect(top.inputWrapWidth).toBe(top.mirrorWrapWidth)
    expect(top.gapShiftOnScroll).toBe(0)
    expect(top.firstLineOffset).toBeGreaterThanOrEqual(0)
    expect(top.firstLineOffset).toBeLessThan(top.clientHeight)
    expect(top.lastLineOffset).toBeGreaterThan(top.clientHeight)

    await wheelDraft(harness, 2_000)
    await waitUntil(() => measureComposer(harness), metrics => metrics.scrollTop > 0)
    const bottom = await measureComposer(harness)
    expect(bottom.caretGlyphGap).toBe(top.caretGlyphGap)
    expect(bottom.gapShiftOnScroll).toBe(0)
    expect(bottom.firstLineOffset).toBeLessThan(0)
    expect(bottom.lastLineOffset).toBeGreaterThanOrEqual(0)
    expect(bottom.lastLineOffset).toBeLessThan(bottom.clientHeight)

    await input.press('End')
    await wheelDraft(harness, -2_000)
    await waitUntil(() => measureComposer(harness), metrics => metrics.scrollTop === 0)
    await input.pressSequentially(' tail')
    const edited = await measureComposer(harness)
    expect(edited.scrollTop).toBeGreaterThan(0)
    expect(edited.lastLineOffset).toBeGreaterThanOrEqual(0)
    expect(edited.lastLineOffset).toBeLessThan(edited.clientHeight)

    await input.fill(`${DRAFT}\n`)
    await wheelDraft(harness, 4_000)
    await waitUntil(() => measureComposer(harness), metrics => metrics.scrollTop === metrics.scrollMax)
    const trailingNewline = await measureComposer(harness)
    expect(trailingNewline.gapShiftOnScroll).toBe(0)
    expect(trailingNewline.lastLineOffset).toBeGreaterThanOrEqual(0)
    expect(trailingNewline.lastLineOffset).toBeLessThan(trailingNewline.clientHeight)

    await input.fill('one short line')
    await input.press('End')
    await input.evaluate((element, text) => {
      const clipboard = new DataTransfer()
      clipboard.setData('text/plain', text)
      element.dispatchEvent(new ClipboardEvent('paste', { clipboardData: clipboard, bubbles: true, cancelable: true }))
    }, `\n${DRAFT}\n`)
    await waitUntil(() => measureComposer(harness), metrics => metrics.overflows && metrics.scrollTop > 0)
    const pasted = await measureComposer(harness)
    expect(pasted.gapShiftOnScroll).toBe(0)
    expect(pasted.lastLineOffset).toBeGreaterThanOrEqual(0)
    expect(pasted.lastLineOffset).toBeLessThan(pasted.clientHeight)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
