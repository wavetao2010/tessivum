import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { acknowledgeReloadConnectionLoss, RustWebHarness, waitUntil } from './support'

const ROOT = 'div[data-phase]'

test('startup materializes and auto-selects blank workspaces without replacing the resident hero or composer', async () => {
  const harness = await RustWebHarness.launch({ name: 'startup-auto-selection' })
  let release = (): void => {}
  try {
    const page = harness.page
    const composer = page.locator('textarea').first()
    await composer.waitFor({ timeout: 15_000 })
    await page.locator(`${ROOT}[data-phase="hero"]`).waitFor({ timeout: 15_000 })
    expect(await composer.isVisible()).toBe(true)
    expect(await page.getByText('Principle and implementation, in concert.', { exact: false }).count()).toBeGreaterThan(0)
    expect((await harness.sessions()).some(session => session.cwd === harness.workspace && session.blank)).toBe(true)

    await page.evaluate(() => {
      const refs = {
        root: document.querySelector('div[data-phase="hero"]'),
        workspace: document.querySelector('[aria-label="Choose workspace"]'),
        scroll: document.querySelector('[data-conversation-scroll]'),
        composer: document.querySelector('[data-composer-seat]'),
        textarea: document.querySelector('textarea'),
      }
      if (Object.values(refs).some(value => value === null)) throw new Error('incomplete resident hero tree')
      Reflect.set(window, '__startupNodes', refs)
    })

    const title = 'startup-auto-selection-second'
    const path = join(harness.root, title)
    await page.getByRole('button', { name: 'Add workspace' }).click()
    const picker = page.getByRole('dialog', { name: 'Select Workspace Directory' })
    await picker.waitFor({ timeout: 10_000 })
    await picker.getByRole('button', { name: 'Edit path' }).click()
    await picker.getByLabel('Edit path').fill(harness.root)
    await picker.getByLabel('Edit path').press('Enter')
    await picker.getByRole('button', { name: 'New folder' }).click()
    await page.getByLabel('Folder name').fill(title)
    await page.getByRole('button', { name: 'Create', exact: true }).click()
    await picker.getByRole('button', { name: 'Open', exact: true }).click()
    await picker.waitFor({ state: 'hidden', timeout: 10_000 })
    await waitUntil(
      () => harness.sessions(),
      sessions => sessions.some(session => session.cwd === path && session.blank),
      15_000,
    )
    expect(await page.evaluate(() => {
      const before = Reflect.get(window, '__startupNodes')
      if (before === null || typeof before !== 'object') throw new Error('resident hero tree was not recorded')
      const textarea = document.querySelector('textarea')
      return {
        phase: document.querySelector('div[data-phase]')?.getAttribute('data-phase'),
        root: document.querySelector('div[data-phase="hero"]') === Reflect.get(before, 'root'),
        workspace: document.querySelector('[aria-label="Choose workspace"]') === Reflect.get(before, 'workspace'),
        scroll: document.querySelector('[data-conversation-scroll]') === Reflect.get(before, 'scroll'),
        composer: document.querySelector('[data-composer-seat]') === Reflect.get(before, 'composer'),
        textarea: textarea === Reflect.get(before, 'textarea'),
        enabled: textarea instanceof HTMLTextAreaElement && !textarea.disabled,
      }
    })).toEqual({ phase: 'hero', root: true, workspace: true, scroll: true, composer: true, textarea: true, enabled: true })

    await page.addInitScript(() => {
      const phases: string[] = []
      Reflect.set(window, '__startupPhases', phases)
      setInterval(() => {
        const phase = document.querySelector('div[data-phase]')?.getAttribute('data-phase')
        if (phase !== null && phase !== undefined && phases.at(-1) !== phase) phases.push(phase)
      }, 8)
    })
    let held = false
    const gate = new Promise<void>(resolve => { release = resolve })
    await page.route('**/api/session.history', async route => {
      const request = route.request().postDataJSON()
      if (!held && request !== null && typeof request === 'object' && 'method' in request && request.method === 'session.history') {
        held = true
        await gate
      }
      await route.continue()
    })
    try {
      const warningsBefore = harness.warnings.length
      await page.reload({ waitUntil: 'commit' })
      await waitUntil(() => Promise.resolve(held), Boolean, 15_000)
      await page.locator(ROOT).waitFor({ timeout: 15_000 })
      expect(await page.locator(ROOT).first().getAttribute('data-phase')).toBe('hero')
      expect(await page.getByText('Principle and implementation, in concert.', { exact: false }).isVisible()).toBe(true)
      expect(await page.locator('textarea').first().isVisible()).toBe(true)

      release()
      await page.locator('textarea:enabled').waitFor({ timeout: 15_000 })
      acknowledgeReloadConnectionLoss(harness, warningsBefore)
      expect(await page.evaluate(() => Reflect.get(window, '__startupPhases'))).toEqual(['hero'])
    } finally {
      release()
      await page.unroute('**/api/session.history')
    }
    harness.assertClean()
  } finally {
    release()
    await harness.close()
  }
}, 120_000)
