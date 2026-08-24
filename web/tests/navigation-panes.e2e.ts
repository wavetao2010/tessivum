import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, fixture, materializeRecording, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/navigation-panes')

const SEED_ID = 'navigation-panes-web-e2e'
const PROMPT_TURN1 = 'NavScenario: first run bash to print exactly NAVIGATION_OK, then read nav-a.md and nav-b.md using two read calls in ONE assistant message, then reply with the single word FIRST_DONE and stop.'
const PROMPT_TURN2 = 'Reply in markdown with: a level-2 heading "Navigation Summary", a bulleted list of exactly two items, and a fenced code block containing echo WATERFALL. Then stop.'

type FixtureRow = {
  type?: string
  data?: {
    source?: { kind?: string }
    content?: Array<{ type?: string; text?: string }>
    name?: string
    commandId?: string
  }
}

function fixtureUserPrompts(document: string): string[] {
  return document.trim().split('\n').flatMap(line => {
    const row = JSON.parse(line) as FixtureRow
    if (row.type !== 'user/message' || row.data?.source?.kind !== 'user' || row.data.content === undefined) return []
    return row.data.content.flatMap(block => block.type === 'text' && typeof block.text === 'string' ? [block.text] : [])
  })
}

function onlyStoredZipEntry(archive: Uint8Array, expectedName: string): string {
  if (archive.byteLength < 52) throw new Error('session export ZIP is truncated')
  const view = new DataView(archive.buffer, archive.byteOffset, archive.byteLength)
  const trailer = archive.byteLength - 22
  expect(view.getUint32(0, true)).toBe(0x04034b50)
  expect(view.getUint16(8, true)).toBe(0)
  expect(view.getUint32(trailer, true)).toBe(0x06054b50)
  expect(view.getUint16(trailer + 10, true)).toBe(1)
  const centralOffset = view.getUint32(trailer + 16, true)
  const nameLength = view.getUint16(26, true)
  const extraLength = view.getUint16(28, true)
  const nameStart = 30
  const payloadStart = nameStart + nameLength + extraLength
  const payloadEnd = payloadStart + view.getUint32(22, true)
  expect(new TextDecoder().decode(archive.subarray(nameStart, nameStart + nameLength))).toBe(expectedName)
  expect(payloadEnd).toBe(centralOffset)
  return new TextDecoder().decode(archive.subarray(payloadStart, payloadEnd))
}

async function snapshot(harness: RustWebHarness, selector: string, pathToken: string): Promise<string> {
  return (await captureStableAria(harness.page, selector))
    .replaceAll(harness.workspace, pathToken)
    .replaceAll(SEED_ID, '{{seededId}}')
}

async function ensureSeedOpen(harness: RustWebHarness): Promise<void> {
  const { page } = harness
  const chat = page.getByRole('tab', { name: 'Chat', exact: true })
  const searchButton = page.getByRole('button', { name: 'Search sessions' })
  if (await searchButton.getAttribute('aria-expanded') !== 'true') await searchButton.click()
  const search = page.getByRole('textbox', { name: 'Search sessions...', exact: true })
  if (await chat.count() === 0) {
    await search.fill('WATERFALL')
    const result = page.getByRole('tree', { name: 'Search results' }).getByRole('treeitem')
    await waitUntil(() => result.count(), count => count === 1, 30_000)
    await result.click()
    await chat.waitFor({ timeout: 30_000 })
  }
  await chat.click()
  await page.getByText('FIRST_DONE', { exact: true }).waitFor({ timeout: 30_000 })
  if (await search.inputValue() !== '') {
    await search.fill('')
    await waitUntil(() => search.inputValue(), value => value === '')
  }
}

test('navigation panes preserve the seeded search, trajectory, export, timeline, and terminal contracts', async () => {
  const seedPath = await fixture('navigation-panes', 'seed.jsonl')
  const searchExpected = join(SNAPSHOT_DIR, 'search-results.expected.md')
  const trajectoryExpected = join(SNAPSHOT_DIR, 'trajectory.expected.md')
  const terminalExpected = join(SNAPSHOT_DIR, 'terminal-card.expected.md')
  const sourceSeed = await readFile(seedPath, 'utf8')
  expect(fixtureUserPrompts(sourceSeed)).toEqual([PROMPT_TURN1, PROMPT_TURN2])
  const harness = await RustWebHarness.launch({
    name: 'navigation-panes-web-e2e', locale: 'en-US', viewport: { width: 1680, height: 1000 },
    beforeStart: async candidate => {
      await mkdir(candidate.workspace, { recursive: true })
      await Promise.all([
        writeFile(join(candidate.workspace, 'nav-a.md'), '# alpha nav\n'),
        writeFile(join(candidate.workspace, 'nav-b.md'), '# beta nav\n'),
      ])
      // The upstream scaffold realizes the fixture's recorded nested cwd as
      // its live workspace before persisting it; the native seed helper owns
      // the same token replacement but not that source-only migration.
      await candidate.seedSession(SEED_ID, materializeRecording(sourceSeed.replaceAll('{{cwd}}/workspace', '{{cwd}}')))
    },
  })
  try {
    const { page } = harness
    const [sessionBaseline, workspaceBaseline] = await Promise.all([
      harness.rpc('session.list'), harness.rpc('workspace.list'),
    ])
    expect(sessionBaseline.ok).toBe(true)
    expect(workspaceBaseline.ok).toBe(true)
    await page.getByRole('treeitem', { name: 'workspace', exact: true }).first().waitFor({ timeout: 30_000 })

    const searchButton = page.getByRole('button', { name: 'Search sessions' })
    if (await searchButton.getAttribute('aria-expanded') !== 'true') await searchButton.click()
    const search = page.getByRole('textbox', { name: 'Search sessions...', exact: true })
    await search.fill('zzzqx-no-such-session')
    await page.getByText('No matching sessions').waitFor({ timeout: 30_000 })
    const resultTree = page.getByRole('tree', { name: 'Search results' })
    const result = resultTree.getByRole('treeitem')
    await waitUntil(() => result.count(), count => count === 0, 10_000)
    await search.fill('WATERFALL')
    await waitUntil(() => result.count(), count => count === 1, 30_000)
    await waitUntil(() => result.getByText('WATERFALL', { exact: false }).count(), count => count >= 1)
    expect(await snapshot(harness, '[class*="listArea"]', '{{workspace}}'))
      .toBe((await readFile(searchExpected, 'utf8')).trim())
    await result.click()
    await waitUntil(() => search.inputValue(), value => value === 'WATERFALL')
    await page.getByText('FIRST_DONE', { exact: true }).waitFor({ timeout: 30_000 })
    await page.getByRole('heading', { name: 'Navigation Summary' }).waitFor({ timeout: 30_000 })
    await page.getByRole('button', { name: 'Clear search' }).click()
    await waitUntil(() => search.inputValue(), value => value === '')
    expect(await page.locator('[role="treeitem"]').count()).toBeGreaterThanOrEqual(1)

    await ensureSeedOpen(harness)
    await page.getByRole('tab', { name: 'Trajectory', exact: true }).click()
    await page.waitForTimeout(100)
    const overlayLayout = await page.getByRole('table').evaluate(table => {
      const host = table.closest('[data-conversation-scroll]')
      const seat = host?.querySelector('[data-composer-seat]')
      const pane = table.parentElement
      return {
        hostPosition: host === null ? null : getComputedStyle(host).position,
        paneOverflowX: pane === null ? null : getComputedStyle(pane).overflowX,
        paneScrollableWidth: pane === null ? null : pane.scrollWidth - pane.clientWidth,
        seatPosition: seat === null ? null : getComputedStyle(seat).position,
      }
    })
    expect(overlayLayout).toEqual({
      hostPosition: 'relative', paneOverflowX: 'hidden', paneScrollableWidth: 0, seatPosition: 'absolute',
    })
    await waitUntil(() => page.locator('tr[data-turn-start="true"]').count(), count => count === 2, 30_000)
    expect(await page.getByRole('columnheader').count()).toBe(0)
    await page.locator('tr[data-kind="tool"]').first().click()
    const details = page.getByRole('complementary', { name: 'Event details' })
    await waitUntil(() => details.count(), count => count === 1)
    expect(await details.getByRole('tabpanel').evaluate(panel => getComputedStyle(panel).overflowX)).toBe('hidden')
    await page.evaluate(() => { document.body.setAttribute('data-ds-dark-theme', '') })
    const darkSummarySurfaces = await details.getByRole('heading', { name: 'Payload' }).evaluate(heading => ({
      heading: getComputedStyle(heading).backgroundColor,
      panel: getComputedStyle(heading.closest('[aria-label="Event details"]')!).backgroundColor,
    }))
    expect(darkSummarySurfaces.heading).toBe(darkSummarySurfaces.panel)
    await page.evaluate(() => { document.body.removeAttribute('data-ds-dark-theme') })
    await details.getByRole('tab', { name: 'Result' }).click()
    await waitUntil(() => page.getByText('NAVIGATION_OK', { exact: false }).count(), count => count >= 1)
    const assistantSpan = page.locator('[data-timeline-span="message"][data-assistant-timing="true"]').first()
    await assistantSpan.hover()
    const timingTooltip = page.getByRole('tooltip')
    await timingTooltip.waitFor({ timeout: 5_000 })
    expect(await timingTooltip.textContent()).toMatch(/TTFT .* Decoding/)
    const assistantTimingStyle = await assistantSpan.evaluate(node => ({
      background: getComputedStyle(node).backgroundImage,
      ttft: getComputedStyle(node).getPropertyValue('--trajectory-assistant-ttft'),
    }))
    expect(assistantTimingStyle.background).toContain('linear-gradient')
    expect(assistantTimingStyle.ttft).toMatch(/%$/)
    expect(await snapshot(harness, '[class*="viewArea"]', '{{cwd}}'))
      .toBe((await readFile(trajectoryExpected, 'utf8')).trim())
    await details.getByRole('button', { name: 'Close details' }).click()

    await ensureSeedOpen(harness)
    const exportButton = page.getByRole('button', { name: 'Session log' })
    expect(await exportButton.isDisabled()).toBe(false)
    const header = exportButton.locator('xpath=ancestor::header[1]')
    const [buttonBox, headerBox] = await Promise.all([exportButton.boundingBox(), header.boundingBox()])
    if (buttonBox === null || headerBox === null) throw new Error('Session Header export geometry is unavailable')
    expect(headerBox.x + headerBox.width - (buttonBox.x + buttonBox.width)).toBeLessThanOrEqual(32)
    const responsePromise = page.waitForResponse(response => response.request().method() === 'HEAD'
      && new URL(response.url()).pathname === '/api/session.export', { timeout: 30_000 })
    const downloadPromise = page.waitForEvent('download', { timeout: 30_000 })
    await exportButton.click()
    expect((await responsePromise).status()).toBe(200)
    const download = await downloadPromise
    expect(download.suggestedFilename()).toMatch(/^dsh-session-.+\.zip$/)
    const dialog = page.getByRole('dialog', { name: 'Session download started' })
    await dialog.waitFor({ timeout: 30_000 })
    const downloadPath = await download.path()
    if (downloadPath === null) throw new Error('session export download has no local path')
    const content = onlyStoredZipEntry(await readFile(downloadPath), 'session.jsonl')
    expect(content.split('\n')[0]).toContain(SEED_ID)
    expect(content).toContain('FIRST_DONE')
    await dialog.getByText('Close', { exact: true }).click()

    const observer = await harness.browser.newPage({ viewport: { width: 1680, height: 1000 }, locale: 'en-US' })
    const observerErrors: string[] = []
    const observerWarnings: string[] = []
    let observerDownloads = 0
    observer.on('pageerror', error => observerErrors.push(error.message))
    observer.on('console', message => {
      if (message.type() === 'warning' || message.type() === 'error') observerWarnings.push(message.text())
    })
    observer.on('download', () => { observerDownloads += 1 })
    try {
      await observer.goto(harness.baseUrl, { waitUntil: 'load' })
      await observer.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
      const observerSearch = observer.getByRole('button', { name: 'Search sessions' })
      if (await observerSearch.getAttribute('aria-expanded') !== 'true') await observerSearch.click()
      await observer.getByRole('textbox', { name: 'Search sessions...', exact: true }).fill('WATERFALL')
      const observerResult = observer.getByRole('tree', { name: 'Search results' }).getByRole('treeitem')
      await waitUntil(() => observerResult.count(), count => count === 1, 30_000)
      await observerResult.click()
      await observer.getByText('FIRST_DONE', { exact: true }).waitFor({ timeout: 30_000 })

      const input = page.locator('textarea').first()
      const slashDownloadPromise = page.waitForEvent('download', { timeout: 30_000 })
      await input.fill('/export')
      await page.getByRole('option', { name: /export/u }).waitFor({ timeout: 10_000 })
      await input.press('Enter')
      const slashDownload = await slashDownloadPromise
      expect(slashDownload.suggestedFilename()).toBe(download.suggestedFilename())
      const slashDownloadPath = await slashDownload.path()
      if (slashDownloadPath === null) throw new Error('slash export download has no local path')
      const slashRows = onlyStoredZipEntry(await readFile(slashDownloadPath), 'session.jsonl')
        .trim().split('\n').map(line => JSON.parse(line) as FixtureRow)
      const exportRun = slashRows.findLast(row => row.type === 'command/run' && row.data?.name === 'export')
      if (exportRun?.data === undefined || typeof exportRun.data.commandId !== 'string') {
        throw new Error('slash ZIP has no export command/run')
      }
      expect(slashRows.some(row => row.type === 'command/done' && row.data?.commandId === exportRun.data?.commandId)).toBe(true)
      await page.getByRole('dialog', { name: 'Session download started' }).waitFor({ timeout: 30_000 })
      await page.getByRole('dialog', { name: 'Session download started' }).getByText('Close', { exact: true }).click()
      await observer.getByText('Session log download requested.', { exact: true }).waitFor({ timeout: 30_000 })
      expect(observerDownloads).toBe(0)
      expect(await observer.getByRole('dialog', { name: 'Session download started' }).count()).toBe(0)
      expect({ observerErrors, observerWarnings }).toEqual({ observerErrors: [], observerWarnings: [] })
    } finally {
      await observer.close()
    }

    await ensureSeedOpen(harness)
    await page.getByRole('tab', { name: 'Trajectory', exact: true }).click()
    const plot = page.getByLabel('Timeline overview; drag horizontally to focus events')
    await plot.waitFor({ timeout: 15_000 })
    const before = await page.locator('tr[data-kind]').count()
    const plotBox = await plot.boundingBox()
    if (plotBox === null) throw new Error('trajectory timeline plot has no layout box')
    await page.mouse.move(plotBox.x + plotBox.width * 0.55, plotBox.y + plotBox.height / 2)
    await page.mouse.down()
    await page.mouse.move(plotBox.x + plotBox.width * 0.9, plotBox.y + plotBox.height / 2)
    await page.mouse.up()
    await waitUntil(() => page.locator('tr[data-timeline-focus="outside"]').count(), count => count > 0)
    await waitUntil(() => page.locator('tr[data-kind]').count(), count => count === before)
    await plot.click({ button: 'right' })
    await waitUntil(() => page.locator('tr[data-timeline-focus]').count(), count => count === 0)

    await ensureSeedOpen(harness)
    const bashRow = page.locator('[data-sample="bash"]').first()
    await bashRow.waitFor({ timeout: 15_000 })
    const frame = page.locator('[style*="grid-template-columns"]').first()
    expect(await frame.getAttribute('data-details-collapsed')).toBe('true')
    await bashRow.click()
    await waitUntil(() => frame.getAttribute('data-details-collapsed'), value => value === 'true')
    await page.locator('[data-sample="bash"] ~ div [data-terminal] [class*="_copyButton_"]').first().click()
    await waitUntil(() => frame.getAttribute('data-details-collapsed'), value => value === 'true')
    const fileLink = page.locator('[data-variant="read"] button').first()
    await fileLink.waitFor({ timeout: 10_000 })
    await fileLink.click()
    await waitUntil(() => frame.getAttribute('data-details-collapsed'), value => value === 'true')

    if (await bashRow.getAttribute('aria-expanded') !== 'true') await bashRow.click()
    const card = page.locator('[data-sample="bash"] ~ div [data-terminal]').first()
    await card.waitFor({ timeout: 15_000 })
    const layout = await card.locator('[class*="_output_"]').first().evaluate(node => {
      const pane = node as HTMLElement
      const row = pane.querySelector<HTMLElement>('[class*="_line_"]')
      if (row === null) throw new Error('output pane has no line')
      const beforeHeight = row.offsetHeight
      const restore = pane.style.width
      pane.style.width = '8px'
      const squeezed = { wrapped: row.offsetHeight > beforeHeight, scrollsSideways: pane.scrollWidth > pane.clientWidth }
      pane.style.width = restore
      return { whiteSpace: getComputedStyle(row).whiteSpace, overflowX: getComputedStyle(pane).overflowX, ...squeezed }
    })
    expect(layout).toEqual({ whiteSpace: 'pre', overflowX: 'auto', wrapped: false, scrollsSideways: true })
    const dot = await card.locator('[class*="_runState_"][data-state]').first().evaluate(node => {
      const stateNode = node as HTMLElement
      const probe = document.createElement('span')
      probe.style.color = 'var(--dsw-alias-state-success-primary)'
      document.body.appendChild(probe)
      const success = getComputedStyle(probe).color
      probe.remove()
      return {
        state: node.getAttribute('data-state'),
        color: getComputedStyle(stateNode).color,
        success,
        label: node.closest('[class*="_prompt_"]')?.querySelector('[class*="_runStateLabel_"]')?.textContent ?? null,
        beforePrompt: node.compareDocumentPosition(node.parentElement!.querySelector('[class*="_cwd_"]')!)
          === Node.DOCUMENT_POSITION_FOLLOWING,
        insideCard: stateNode.getBoundingClientRect().left
          >= (node.closest('[data-terminal]')?.getBoundingClientRect().left ?? Infinity),
        leftOfPrompt: stateNode.getBoundingClientRect().right
          <= (node.closest('[class*="_promptLine_"]')?.querySelector('[class*="_cwd_"]')?.getBoundingClientRect().left ?? -Infinity),
      }
    })
    expect(dot.state).toBe('done')
    expect(dot.label).toBe('Done')
    expect(dot.beforePrompt).toBe(true)
    expect(dot.insideCard).toBe(true)
    expect(dot.leftOfPrompt).toBe(true)
    expect(dot.success).toMatch(/^rgb/)
    expect(dot.color).toBe(dot.success)
    await waitUntil(() => card.locator('[class*="_copyButton_"]').first().textContent(), text => text === 'Copy', 5_000)
    expect(await snapshot(harness, '[data-terminal]', '{{workspace}}'))
      .toBe((await readFile(terminalExpected, 'utf8')).trim())
    await page.context().grantPermissions(['clipboard-read', 'clipboard-write'])
    await card.locator('[class*="_copyButton_"]').first().click()
    await waitUntil(() => card.locator('[class*="_copyButton_"]').first().textContent(), text => text === 'Copied')
    expect(await page.evaluate(() => navigator.clipboard.readText())).toContain('NAVIGATION_OK')

    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
      'search-results.expected.md', 'terminal-card.expected.md', 'trajectory.expected.md',
    ].sort())
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 480_000)
