import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import {
  acknowledgeReloadConnectionLoss, fixture, materializeRecording, openSessionByMarker, RustWebHarness, waitUntil,
} from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/details-session-lifecycle')
const HANDLES_EXPECTED = join(SNAPSHOT_DIR, 'handles.expected.md')
const FIXTURE = join(import.meta.dir, 'snapshots/lifecycle-chrome/session.jsonl')
const PROMPT = 'Reply with the single word LIGHTHOUSE and stop.'
const SEEDED_PROMPT = 'Use the read tool twice in one assistant message: read a.txt and b.txt. Then reply with the single word DONE and stop.'
const SEEDED_ID = 'details-session-lifecycle-seed'

async function fixtureUserPrompts(): Promise<string[]> {
  return (await readFile(FIXTURE, 'utf8'))
    .trim()
    .split('\n')
    .map(line => JSON.parse(line) as { type?: string; data?: { content?: Array<{ type?: string; text?: string }> } })
    .filter(row => row.type === 'user/message')
    .flatMap(row => row.data?.content ?? [])
    .flatMap(block => block.type === 'text' && block.text !== undefined ? [block.text] : [])
}

async function detailsTrack(harness: RustWebHarness): Promise<number> {
  return harness.page.locator('[style*="grid-template-columns"]').first().evaluate((element) => {
    const tracks = getComputedStyle(element).gridTemplateColumns.split(' ')
    return Number.parseFloat(tracks.at(-1) ?? 'NaN')
  })
}

async function sidebarTrack(harness: RustWebHarness): Promise<number> {
  return harness.page.locator('[style*="grid-template-columns"]').first().evaluate((element) => {
    const tracks = getComputedStyle(element).gridTemplateColumns.split(' ')
    return Number.parseFloat(tracks[0] ?? 'NaN')
  })
}

async function handleSnapshot(harness: RustWebHarness): Promise<string> {
  const handles = await harness.page.locator('[class*="handle"]').evaluateAll(elements =>
    elements.map(element => ({
      side: element.getAttribute('data-side'),
      cursor: getComputedStyle(element).cursor,
      pillGenerated: getComputedStyle(element, '::after').content !== 'none',
    })))
  return [
    '# AppFrame drag handles',
    '',
    ...handles.flatMap(handle => [
      `## ${handle.side}`,
      '',
      '- hit strip present: true',
      `- cursor: ${handle.cursor}`,
      `- pill generated: ${String(handle.pillGenerated)}`,
      '',
    ]),
  ].join('\n').trimEnd()
}

test('details panel starts and reloads closed across Session ownership changes', async () => {
  expect(await fixtureUserPrompts()).toEqual([PROMPT])
  const seeded = materializeRecording(await readFile(await fixture('seeded-history', 'seed.jsonl'), 'utf8'))
  const harness = await RustWebHarness.launch({
    name: 'details-session-lifecycle',
    locale: 'en-US',
    replayFixture: FIXTURE,
    beforeStart: candidate => candidate.seedSession(SEEDED_ID, seeded),
  })
  try {
    const input = harness.page.locator('textarea').first()
    const settled = harness.whenTurnSettled()
    await input.fill(PROMPT)
    await input.press('Enter')
    await settled
    await harness.page.getByText('LIGHTHOUSE', { exact: true }).waitFor({ timeout: 15_000 })

    await waitUntil(() => detailsTrack(harness), track => track === 0, 5_000)
    expect(await harness.page.getByText('Details', { exact: true }).isVisible()).toBe(false)
    expect(await handleSnapshot(harness)).toBe((await readFile(HANDLES_EXPECTED, 'utf8')).trim())

    const sidebarBefore = await sidebarTrack(harness)
    const sidebarHandle = harness.page.locator('[data-side="sidebar"]')
    const sidebarBox = await sidebarHandle.boundingBox()
    expect(sidebarBox).not.toBeNull()
    const dragStartX = sidebarBox!.x + sidebarBox!.width / 2
    await harness.page.mouse.move(dragStartX, sidebarBox!.y + 200)
    await harness.page.mouse.down()
    await harness.page.mouse.move(dragStartX + 70, sidebarBox!.y + 200, { steps: 6 })
    await harness.page.mouse.up()
    await waitUntil(() => sidebarTrack(harness), track => track === sidebarBefore + 70, 5_000)

    const warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await harness.page.locator('[style*="grid-template-columns"]').first().waitFor({ timeout: 30_000 })
    await harness.page.getByText('LIGHTHOUSE', { exact: true }).waitFor({ timeout: 15_000 })
    await waitUntil(() => detailsTrack(harness), track => track === 0, 5_000)
    expect(await harness.page.getByText('Details', { exact: true }).isVisible()).toBe(false)

    await harness.page.getByRole('button', { name: /^(?:New session|新.*会话)$/ }).last().click()
    await harness.page.getByText('Into the Unknown', { exact: false }).waitFor({ timeout: 15_000 })
    await waitUntil(() => detailsTrack(harness), track => track === 0, 5_000)
    expect(await harness.page.getByText('Details', { exact: true }).isVisible()).toBe(false)

    await harness.page.getByRole('treeitem').filter({ hasText: 'Reply with the single word' }).first().click()
    await harness.page.getByText('LIGHTHOUSE', { exact: true }).waitFor({ timeout: 15_000 })
    await waitUntil(() => detailsTrack(harness), track => track === 0, 5_000)
    expect(await harness.page.getByText('Details', { exact: true }).isVisible()).toBe(false)

    await openSessionByMarker(harness, SEEDED_PROMPT, 'DONE')
    await waitUntil(() => detailsTrack(harness), track => track === 0, 5_000)
    const seededHistory = await harness.rpc<{ events: Array<{ event: { type: string } }> }>(
      'session.history',
      { sessionId: SEEDED_ID, maxMessages: 1_000 },
    )
    expect(seededHistory.ok).toBe(true)
    const seededTypes = seededHistory.value?.events.map(entry => entry.event.type) ?? []
    expect(seededTypes.filter(type => type === 'request/header').length).toBeGreaterThan(0)
    expect(seededTypes.filter(type => type === 'tool/call')).toHaveLength(2)
    expect(seededTypes.filter(type => type === 'tool/result')).toHaveLength(2)
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['handles.expected.md'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 90_000)
