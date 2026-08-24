import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

const tokens = Array.from({ length: 220 }, (_, index) => `tok${((index + 1) * 7919 % 99991).toString(36)}`).join(' ')
const prompt = `Write a file named notes.txt in the workspace containing exactly this text on one line: ${tokens}. Use one bash command with the literal text inline. Then reply with the single word DONE and stop.`

function approvalReplay(text: string): string {
  const argumentsJson = JSON.stringify({
    command: `echo '${text}' > notes.txt`,
    description: 'Write notes.txt with the requested text',
    sandbox_permissions: 'workspace-write',
    justification: 'Need to write the file requested by the user.',
  })
  const chunks = [
    { type: 'block-start', index: 0, blockType: 'tool-call' },
    { type: 'block-end', index: 0, block: { type: 'tool-call', id: 'approval-call', name: 'bash', arguments: argumentsJson } },
    { type: 'finish', reason: { kind: 'tool-calls' } },
    { type: 'block-start', index: 0, blockType: 'text' },
    { type: 'block-end', index: 0, block: { type: 'text', text: 'DONE' } },
    { type: 'finish', reason: { kind: 'stop' } },
  ]
  return [
    { type: 'session', version: 0, id: 'approval-replay', createdAt: 0, cwd: '{{cwd}}' },
    ...chunks.map((chunk, seq) => ({
      type: 'assistant/chunk', seq, time: 0, data: { turn: 1, step: seq < 3 ? 1 : 2, chunk },
    })),
  ].map(value => JSON.stringify(value)).join('\n')
}

test('keeps a sandbox escalation approval reachable and runs it only after consent', async () => {
  const harness = await RustWebHarness.launch({
    name: 'approval-composer-web-e2e', locale: 'en-US', replayRecording: approvalReplay(tokens),
  })
  try {
    const input = harness.page.locator('textarea').first()
    await harness.page.locator('[aria-label^="Access mode"]').click()
    await harness.page.getByRole('menuitem', { name: 'Read Only' }).click()
    await expect(waitUntil(() => harness.page.locator('[aria-label="Access mode, current: Read Only"]').count(), count => count === 1)).resolves.toBe(1)
    const settled = harness.whenTurnSettled()

    await input.fill(prompt)
    await input.press('Enter')

    const panel = harness.page.locator('[data-approval-key]')
    await panel.waitFor({ timeout: 60_000 })
    const scroll = panel.locator('[data-approval-scroll]')
    await expect(waitUntil(() => scroll.getByText(/tok/).count(), count => count > 0)).resolves.toBeGreaterThan(0)
    await expect(readFile(join(harness.workspace, 'notes.txt'), 'utf8')).rejects.toThrow()

    const geometry = await panel.evaluate(root => {
      const region = root.querySelector<HTMLElement>('[data-approval-scroll]')
      const buttons = [...root.querySelectorAll<HTMLElement>('button')].map(button => button.getBoundingClientRect())
      return {
        buttons: buttons.length,
        scrolls: region !== null && region.scrollHeight > region.clientHeight,
        actionsBottom: Math.max(...buttons.map(rect => rect.bottom)),
        viewport: window.innerHeight,
      }
    })
    expect(geometry.buttons).toBe(2)
    expect(geometry.scrolls).toBe(true)
    expect(geometry.actionsBottom).toBeLessThanOrEqual(geometry.viewport)

    const response = harness.page.waitForResponse(value => value.url().endsWith('/api/respond'), { timeout: 10_000 })
    await panel.getByRole('button', { name: 'Allow once' }).click({ timeout: 5_000 })
    expect(await (await response).json()).toEqual({ accepted: true })
    await panel.waitFor({ state: 'detached', timeout: 15_000 })
    await settled
    expect(await readFile(join(harness.workspace, 'notes.txt'), 'utf8')).toBe(`${tokens}\n`)
    await expect(waitUntil(() => harness.page.getByText('DONE', { exact: true }).count(), count => count > 0)).resolves.toBeGreaterThanOrEqual(1)
    expect(await panel.count()).toBe(0)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
