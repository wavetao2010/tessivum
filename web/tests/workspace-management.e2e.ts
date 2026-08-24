import { expect, test } from 'bun:test'
import { mkdir, stat } from 'node:fs/promises'
import { join, sep } from 'node:path'
import type { Locator } from 'playwright-core'
import {
  acknowledgeReloadConnectionLoss, captureStableAria, RustWebHarness, settledRecording, UPSTREAM_TESTS, waitUntil,
} from './support'

interface Workspace {
  workspaceId: string
  path: string
  title: string
}

async function workspaceItems(harness: RustWebHarness): Promise<{ items: Workspace[]; archivedSessionIds: string[] }> {
  const result = await harness.rpc<{ items: Workspace[]; archivedSessionIds: string[] }>('workspace.list')
  if (!result.ok || result.value === undefined) throw new Error(`workspace.list failed: ${JSON.stringify(result.error)}`)
  return result.value
}

async function openWorkspaceAction(row: Locator, name: string): Promise<void> {
  await row.hover()
  await row.evaluate((element, label) => {
    const button = [...element.querySelectorAll<HTMLButtonElement>('button')]
      .find(candidate => candidate.getAttribute('aria-label') === label)
    if (button === undefined) throw new Error(`workspace action not found: ${label}`)
    button.click()
  }, name)
}

async function browseTo(harness: RustWebHarness, path: string): Promise<Locator> {
  await harness.page.getByRole('button', { name: 'Add workspace' }).click()
  const dialog = harness.page.getByRole('dialog', { name: 'Select Workspace Directory' })
  await dialog.waitFor({ timeout: 10_000 })
  await dialog.getByRole('button', { name: 'Edit path' }).click()
  await dialog.getByLabel('Edit path').fill(path)
  await dialog.getByLabel('Edit path').press('Enter')
  return dialog
}

async function addNewFolderWorkspace(harness: RustWebHarness, parent: string, name: string): Promise<Workspace> {
  const dialog = await browseTo(harness, parent)
  await dialog.getByRole('button', { name: 'New folder' }).click()
  await harness.page.getByLabel('Folder name').fill(name)
  await harness.page.getByRole('button', { name: 'Create', exact: true }).click()
  await dialog.getByRole('button', { name: 'Open', exact: true }).click()
  await dialog.waitFor({ state: 'hidden', timeout: 10_000 })
  const path = join(parent, name)
  const listed = await waitUntil(() => workspaceItems(harness), result => result.items.some(item => item.path === path))
  const workspace = listed.items.find(item => item.path === path)
  if (workspace === undefined) throw new Error('workspace did not materialize through directory chrome')
  return workspace
}

async function adoptDirectory(harness: RustWebHarness, path: string): Promise<Workspace> {
  const dialog = await browseTo(harness, path)
  await dialog.getByRole('button', { name: 'Open', exact: true }).click()
  await dialog.waitFor({ state: 'hidden', timeout: 10_000 })
  const listed = await waitUntil(() => workspaceItems(harness), result => result.items.some(item => item.path === path))
  const workspace = listed.items.find(item => item.path === path)
  if (workspace === undefined) throw new Error('workspace did not materialize through directory chrome')
  return workspace
}

test('workspace creation, browsing, view preferences, and safe deletion survive the shipped chrome', async () => {
  const originalHome = process.env.HOME
  const preservedId = 'workspace-delete-preserved'
  const preservedTitle = 'Preserved workspace session'
  let harness: RustWebHarness | undefined
  try {
    harness = await RustWebHarness.launch({
      name: 'workspace-management',
      beforeStart: async candidate => {
        process.env.HOME = candidate.root
        await candidate.seedSession(
          preservedId,
          settledRecording(preservedTitle, 'Keep this session.', 'PRESERVED_SESSION_DONE')
            .replaceAll('{{cwd}}', join(candidate.root, 'alpha-ws')),
        )
      },
    })
  } finally {
    if (originalHome === undefined) delete process.env.HOME
    else process.env.HOME = originalHome
  }
  if (harness === undefined) throw new Error('workspace harness did not launch')
  try {
    const alpha = await addNewFolderWorkspace(harness, harness.root, 'alpha-ws')
    const beta = await addNewFolderWorkspace(harness, harness.root, 'beta-ws')
    expect((await workspaceItems(harness)).items.slice(0, 2).map(workspace => workspace.title)).toEqual(['beta-ws', 'alpha-ws'])

    let alphaRow = harness.page.locator('[role="treeitem"]').filter({ hasText: 'alpha-ws' }).first()
    await alphaRow.waitFor({ timeout: 10_000 })
    await openWorkspaceAction(alphaRow, 'Workspace actions for alpha-ws')
    await harness.page.getByRole('menuitem', { name: 'Rename' }).click()
    const rename = harness.page.getByRole('dialog', { name: 'Rename workspace' })
    const input = rename.getByLabel('Workspace name')
    await input.fill(beta.title)
    expect(await waitUntil(() => rename.getByRole('alert').count(), count => count === 1)).toBe(1)
    expect(await rename.getByRole('button', { name: 'Rename' }).isDisabled()).toBe(true)
    await input.fill('gamma-ws')
    expect(await waitUntil(() => rename.getByRole('alert').count(), count => count === 0)).toBe(0)
    await rename.getByRole('button', { name: 'Rename' }).click()
    await rename.waitFor({ state: 'detached', timeout: 10_000 })
    alphaRow = harness.page.locator('[role="treeitem"]').filter({ hasText: 'gamma-ws' }).first()
    await alphaRow.waitFor({ timeout: 10_000 })
    expect((await workspaceItems(harness)).items.find(item => item.workspaceId === alpha.workspaceId)?.title).toBe('gamma-ws')
    const attached = await harness.rpc('workspace.insertSessionBefore', {
      workspaceId: alpha.workspaceId, sessionId: preservedId, beforeSessionId: null,
    })
    expect(attached.ok).toBe(true)
    const preservedLog = join(harness.dataDir, `session-${Buffer.from(preservedId).toString('hex')}.jsonl`)
    await stat(preservedLog)
    const afterAttachWarnings = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, afterAttachWarnings)
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    if (await alphaRow.getAttribute('aria-expanded') !== 'true') await alphaRow.click()
    const preservedRow = harness.page.locator('[role="treeitem"]').filter({ hasText: preservedTitle }).first()
    await preservedRow.waitFor({ timeout: 10_000 })
    await preservedRow.click()
    expect(await waitUntil(() => preservedRow.getAttribute('aria-selected'), value => value === 'true')).toBe('true')

    await openWorkspaceAction(alphaRow, 'Workspace actions for gamma-ws')
    await harness.page.getByRole('menuitem', { name: 'Delete workspace' }).click()
    const deletion = harness.page.getByRole('dialog', { name: 'Delete workspace' })
    await deletion.waitFor({ timeout: 10_000 })
    const copy = await deletion.textContent()
    expect(copy).toContain('workspace list')
    expect(copy).toContain('folder and session logs will be kept')
    expect(copy).toContain('sessions will appear under Ungrouped')
    await deletion.getByRole('button', { name: 'Delete workspace' }).click()
    await deletion.waitFor({ state: 'detached', timeout: 10_000 })
    expect(await waitUntil(() => harness.page.getByRole('button', { name: 'Workspace actions for gamma-ws' }).count(), count => count === 0)).toBe(0)
    expect((await workspaceItems(harness)).items.some(item => item.workspaceId === alpha.workspaceId)).toBe(false)
    await stat(alpha.path)
    expect(await waitUntil(() => preservedRow.getAttribute('aria-selected'), value => value === 'true')).toBe('true')
    await harness.page.getByText('Ungrouped', { exact: true }).waitFor({ timeout: 10_000 })
    const preservedHistory = await harness.rpc<{ events: unknown[] }>('session.history', { sessionId: preservedId, maxMessages: 1_000 })
    expect(preservedHistory.value?.events.length).toBeGreaterThan(0)
    const reregistered = await adoptDirectory(harness, alpha.path)
    expect(reregistered.workspaceId).not.toBe(alpha.workspaceId)
    const reregisteredRow = harness.page.locator('[role="treeitem"]').filter({ hasText: 'alpha-ws' }).first()
    await reregisteredRow.waitFor({ timeout: 10_000 })
    await openWorkspaceAction(reregisteredRow, 'Workspace actions for alpha-ws')
    await harness.page.getByRole('menuitem', { name: 'Delete workspace' }).click()
    await harness.page.getByRole('dialog', { name: 'Delete workspace' })
      .getByRole('button', { name: 'Delete workspace' }).click()
    expect(await waitUntil(async () => (await workspaceItems(harness)).items.some(item => item.workspaceId === reregistered.workspaceId), present => !present)).toBe(false)
    await stat(preservedLog)

    let warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    expect(await harness.page.getByRole('button', { name: 'Workspace actions for gamma-ws' }).count()).toBe(0)
    await harness.page.getByText('Ungrouped', { exact: true }).waitFor({ timeout: 10_000 })
    expect(await harness.page.locator('[role="treeitem"][aria-selected="true"]').count()).toBe(1)
    await stat(alpha.path)

    const oldPath = join(harness.root, 'adopted', 'same-name')
    await mkdir(oldPath, { recursive: true })
    const oldWorkspace = await adoptDirectory(harness, oldPath)
    const oldRow = harness.page.locator('[role="treeitem"]').filter({ hasText: 'same-name' }).first()
    await oldRow.waitFor({ timeout: 10_000 })
    await openWorkspaceAction(oldRow, 'Workspace actions for same-name')
    await harness.page.getByRole('menuitem', { name: 'Delete workspace' }).click()
    await harness.page.getByRole('dialog', { name: 'Delete workspace' })
      .getByRole('button', { name: 'Delete workspace' }).click()
    expect(await waitUntil(async () => (await workspaceItems(harness)).items.some(item => item.workspaceId === oldWorkspace.workspaceId), present => !present)).toBe(false)
    const sameName = await addNewFolderWorkspace(harness, harness.root, 'same-name')
    expect(sameName.workspaceId).not.toBe(oldWorkspace.workspaceId)
    expect(sameName.path).toBe(join(harness.root, 'same-name'))

    await harness.page.getByRole('button', { name: 'View options' }).click()
    await harness.page.getByRole('menuitem', { name: 'In one list' }).click()
    await harness.page.getByText('Sessions', { exact: true }).waitFor({ timeout: 10_000 })
    expect(await harness.page.evaluate(() => localStorage.getItem('dsh.workspace.view.v5'))).toContain('flat')
    warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    expect(await harness.page.getByText('Ungrouped', { exact: true }).count()).toBe(0)
    await harness.page.getByRole('button', { name: 'View options' }).click()
    await harness.page.getByRole('menuitem', { name: 'WorkSpace' }).click()
    await harness.page.getByText('Workspaces', { exact: true }).waitFor({ timeout: 10_000 })

    const staged = join(harness.root, 'browse-golden')
    await mkdir(join(staged, 'alpha'), { recursive: true })
    await mkdir(join(staged, 'beta'), { recursive: true })
    const browser = await browseTo(harness, staged)
    await browser.getByText('alpha', { exact: true }).waitFor({ timeout: 10_000 })
    const browserGolden = join(UPSTREAM_TESTS, 'snapshots/workspace-management/directory-browser.expected.md')
    expect(await captureStableAria(harness.page, '[role="dialog"]')).toBe((await Bun.file(browserGolden).text()).trim())
    await browser.getByRole('button', { name: 'Cancel' }).click()
    await browser.waitFor({ state: 'hidden', timeout: 10_000 })

    await mkdir(join(staged, 'alpha', 'only-under-alpha'), { recursive: true })
    const panes = await browseTo(harness, staged)
    await panes.getByText('alpha', { exact: true }).waitFor({ timeout: 10_000 })
    await panes.getByRole('button', { name: 'Edit path' }).click()
    const path = panes.getByLabel('Edit path')
    await path.fill(`${join(staged, 'alpha')}${sep}`)
    await panes.getByText('only-under-alpha', { exact: true }).waitFor({ timeout: 10_000 })
    expect(await panes.getByRole('list').count()).toBe(2)
    expect(await path.inputValue()).toBe(`${join(staged, 'alpha')}${sep}`)
    await path.fill(`${staged}${sep}al`)
    expect(await waitUntil(() => panes.getByText('only-under-alpha', { exact: true }).count(), count => count === 0)).toBe(0)
    expect(await panes.getByText('alpha', { exact: true }).count()).toBe(1)
    expect(await panes.getByText('beta', { exact: true }).count()).toBe(0)
    expect(await panes.getByRole('list').count()).toBe(2)
    await path.fill(`${staged}${sep}zzz`)
    await panes.getByText('beta', { exact: true }).waitFor({ timeout: 10_000 })
    expect(await panes.getByText('alpha', { exact: true }).count()).toBe(1)
    await panes.getByRole('button', { name: 'Cancel' }).click()
    await panes.waitFor({ state: 'hidden', timeout: 10_000 })

    const seedId = 'workspace-management-seed'
    const seedTitle = 'Seeded workspace session'
    await harness.seedSession(seedId, settledRecording(seedTitle, 'Workspace management seed.', 'WORKSPACE_MANAGEMENT_DONE'))
    warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    const ungrouped = harness.page.locator('[role="treeitem"]').filter({ hasText: 'Ungrouped' }).first()
    await ungrouped.waitFor({ timeout: 10_000 })
    if (await ungrouped.getAttribute('aria-expanded') !== 'true') await ungrouped.click()
    const seededRow = harness.page.locator('[role="treeitem"]').filter({ hasText: seedTitle }).first()
    await seededRow.waitFor({ timeout: 10_000 })
    await seededRow.hover()
    const card = harness.page.getByRole('button', { name: `Copy: ${seedTitle}` })
    await card.waitFor({ timeout: 10_000 })
    expect(await harness.page.getByText('Idle', { exact: true }).count()).toBeGreaterThanOrEqual(1)
    await card.hover()
    await harness.page.context().grantPermissions(['clipboard-read', 'clipboard-write'])
    const cardHeight = (await card.boundingBox())?.height
    await card.click()
    await harness.page.getByRole('status').getByText('Copied', { exact: true }).waitFor({ timeout: 10_000 })
    expect((await card.boundingBox())?.height).toBe(cardHeight)
    expect(await harness.page.evaluate(() => navigator.clipboard.readText())).toBe(seedTitle)
    await harness.page.getByRole('button', { name: 'Settings' }).hover()
    expect(await waitUntil(() => card.count(), count => count === 0)).toBe(0)

    const actionName = `Session actions for ${seedTitle}`
    await openWorkspaceAction(seededRow, actionName)
    const renameItem = harness.page.getByRole('menuitem', { name: 'Rename' })
    await renameItem.waitFor({ timeout: 10_000 })
    await renameItem.hover()
    await Bun.sleep(300)
    await seededRow.getByRole('button', { name: actionName }).hover()
    await Bun.sleep(600)
    expect(await renameItem.count()).toBe(1)
    await renameItem.hover()
    await Bun.sleep(600)
    expect(await renameItem.count()).toBe(1)
    await harness.page.getByRole('button', { name: 'Settings' }).hover()
    expect(await waitUntil(() => renameItem.count(), count => count === 0)).toBe(0)

    await openWorkspaceAction(seededRow, actionName)
    await harness.page.getByRole('menuitem', { name: 'Archive session' }).click()
    const archived = await waitUntil(
      () => workspaceItems(harness),
      snapshot => snapshot.archivedSessionIds.includes(seedId),
    )
    expect(archived.archivedSessionIds).toEqual([seedId])
    warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    expect(await harness.page.getByText(seedTitle, { exact: true }).count()).toBe(0)

    const firstPath = join(harness.root, 'same-basename-a', 'xx')
    const secondPath = join(harness.root, 'same-basename-b', 'xx')
    await mkdir(firstPath, { recursive: true })
    await mkdir(secondPath, { recursive: true })
    await adoptDirectory(harness, firstPath)
    await adoptDirectory(harness, secondPath)
    const matching = (await workspaceItems(harness)).items.filter(workspace => workspace.title === 'xx')
    expect(matching.map(workspace => workspace.path).sort()).toEqual([firstPath, secondPath].sort())
    expect(await harness.page.locator('button[aria-label="Workspace actions for xx"]').count()).toBe(2)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 180_000)
