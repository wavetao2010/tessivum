import { expect, test } from 'bun:test'
import { openSnapshotSession, seedSnapshotFixture } from './snapshot-fixture'
import { RustWebHarness, waitUntil } from './support'


test('renders the history image pair through the authorized attachment route and opens the lightbox', async () => {
  const harness = await RustWebHarness.launch({ name: 'image-display-history', env: { DEEPSEEK_API_KEY: 'test' } })
  try {
    await seedSnapshotFixture(harness)
    await openSnapshotSession(harness)
    const galleryShape = async (align: string) => harness.page.locator(`[data-align="${align}"] img`).evaluateAll(images => images.map(image => ({
      alt: image.getAttribute('alt'), scheme: image.getAttribute('src')?.split(':')[0],
    })))
    expect({ user: await galleryShape('end'), assistant: await galleryShape('start') }).toEqual({
      user: [{ alt: 'fixture-image.png', scheme: 'blob' }],
      assistant: [{ alt: 'fixture-image.png', scheme: 'blob' }],
    })
    const userImage = harness.page.locator('[data-align="end"] img').first()
    const frame = userImage.locator('xpath=ancestor::button[1]')
    await frame.click()
    const lightbox = harness.page.getByRole('dialog')
    await lightbox.waitFor({ timeout: 10_000 })
    expect(await lightbox.getByRole('img', { name: 'fixture-image.png' }).getAttribute('src')).toMatch(/^blob:/)
    await lightbox.getByRole('button', { name: /Close/ }).click()
    expect(await waitUntil(() => harness.page.getByRole('dialog').count(), count => count === 0)).toBe(0)
    harness.assertClean()
  } finally {
    await harness.close()
  }
})

test('accepts pasted images into the composer rail in order and removes them', async () => {
  const harness = await RustWebHarness.launch({ name: 'image-display-paste', env: { DEEPSEEK_API_KEY: 'test' } })
  try {
    await seedSnapshotFixture(harness)
    await openSnapshotSession(harness)
    const textarea = harness.page.locator('textarea').first()
    await textarea.evaluate(element => {
      const bytes = Uint8Array.from(atob('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII='), character => character.charCodeAt(0))
      const file = new File([bytes], 'pasted.png', { type: 'image/png' })
      const event = new Event('paste', { bubbles: true, cancelable: true })
      Object.defineProperty(event, 'clipboardData', { value: { items: [{ kind: 'file', type: file.type, getAsFile: () => file }], getData: () => '' } })
      element.dispatchEvent(event)
    })
    const rail = harness.page.getByRole('group', { name: 'Pending images' })
    await rail.waitFor({ timeout: 10_000 })
    expect(await rail.locator('img').evaluateAll(images => images.map(image => ({ alt: image.getAttribute('alt'), scheme: image.getAttribute('src')?.split(':')[0] })))).toEqual([{ alt: 'pasted.png', scheme: 'blob' }])
    await textarea.evaluate(element => {
      const file = new File([new Uint8Array([137, 80, 78, 71])], 'second.png', { type: 'image/png' })
      const event = new Event('paste', { bubbles: true, cancelable: true })
      Object.defineProperty(event, 'clipboardData', { value: { items: [{ kind: 'file', type: file.type, getAsFile: () => file }], getData: () => '' } })
      element.dispatchEvent(event)
    })
    await waitUntil(() => rail.locator('img').count(), count => count === 2)
    expect(await rail.locator('img').evaluateAll(images => images.map(image => image.getAttribute('alt')))).toEqual(['pasted.png', 'second.png'])
    await rail.getByRole('button', { name: /^Remove image/ }).first().click()
    await rail.getByRole('button', { name: /^Remove image/ }).first().click()
    expect(await waitUntil(() => harness.page.getByRole('group', { name: 'Pending images' }).count(), count => count === 0)).toBe(0)
    await textarea.evaluate(element => {
      const file = new File(['x'], 'notes.txt', { type: 'text/plain' })
      const event = new Event('paste', { bubbles: true, cancelable: true })
      Object.defineProperty(event, 'clipboardData', { value: { items: [{ kind: 'file', type: file.type, getAsFile: () => file }], getData: () => '' } })
      element.dispatchEvent(event)
    })
    const toast = harness.page.getByRole('alert')
    await toast.waitFor({ timeout: 10_000 })
    expect(await toast.textContent()).toContain('Only PNG, JPG, WebP, and GIF images are supported')
    expect(await waitUntil(() => toast.count(), count => count === 0, 6_000)).toBe(0)
    harness.assertClean()
  } finally {
    await harness.close()
  }
})

test('accepts a whole-page drop under the limits-labeled overlay and refuses an over-limit batch at intake', async () => {
  const harness = await RustWebHarness.launch({ name: 'image-display-drop', env: { DEEPSEEK_API_KEY: 'test' } })
  try {
    await seedSnapshotFixture(harness)
    await openSnapshotSession(harness)
    await harness.page.evaluate(() => {
      const event = new Event('dragenter', { bubbles: true, cancelable: true })
      Object.defineProperty(event, 'dataTransfer', { value: { types: ['Files'], files: [], dropEffect: 'none' } })
      document.body.dispatchEvent(event)
    })
    const overlay = harness.page.getByRole('status').filter({ hasText: 'Drag images here to add them' })
    await overlay.waitFor({ timeout: 10_000 })
    expect(await overlay.textContent()).toContain('Drag images here to add them')
    expect(await overlay.textContent()).toContain('Up to 20 images, 5MB each')
    await harness.page.evaluate(() => {
      const file = new File([new Uint8Array([137, 80, 78, 71])], 'dropped.png', { type: 'image/png' })
      const event = new Event('drop', { bubbles: true, cancelable: true })
      Object.defineProperty(event, 'dataTransfer', { value: { types: ['Files'], files: [file], dropEffect: 'none' } })
      document.body.dispatchEvent(event)
    })
    const rail = harness.page.getByRole('group', { name: 'Pending images' })
    await rail.waitFor({ timeout: 10_000 })
    expect(await rail.locator('img').evaluateAll(images => images.map(image => image.getAttribute('alt')))).toEqual(['dropped.png'])
    expect(await waitUntil(() => overlay.count(), count => count === 0)).toBe(0)
    const textarea = harness.page.locator('textarea').first()
    await textarea.evaluate(element => {
      const files = Array.from({ length: 20 }, (_, i) => new File([new Uint8Array([137, 80, 78, 71])], `bulk-${i}.png`, { type: 'image/png' }))
      const event = new Event('paste', { bubbles: true, cancelable: true })
      Object.defineProperty(event, 'clipboardData', { value: { items: files.map(file => ({ kind: 'file', type: file.type, getAsFile: () => file })), getData: () => '' } })
      element.dispatchEvent(event)
    })
    const banner = harness.page.getByRole('alert')
    await banner.waitFor({ timeout: 10_000 })
    expect(await banner.textContent()).toContain('A message can include up to 20 images')
    expect(await rail.locator('img').count()).toBe(1)
    harness.assertClean()
  } finally {
    await harness.close()
  }
})
