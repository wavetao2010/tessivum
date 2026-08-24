import { spawnSync } from 'node:child_process'
import { readdir, readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/pwsh-terminal')
const SEED = join(SNAPSHOT_DIR, 'seed.jsonl')
const TERMINAL_EXPECTED = join(SNAPSHOT_DIR, 'terminal-card.expected.md')
const PROMPT = 'Run a PowerShell command that fails, then stop.'
const DONE = 'PWSH_NATIVE_TERMINAL_DONE'
const HAS_PWSH = spawnSync('pwsh', ['-NoLogo', '-NoProfile', '-NonInteractive', '-Command', '$true'], {
  encoding: 'utf8',
}).status === 0

function replayRecording(): string {
  const argumentsJson = JSON.stringify({
    command: 'pwsh -NoLogo -NoProfile -NonInteractive -Command "Get-Item missing.txt"',
    description: 'Fail deliberately',
  })
  const attempts = [
    [
      { type: 'block-start', index: 0, blockType: 'tool-call' },
      { type: 'tool-call-delta', index: 0, id: 'pwsh-native-fail', name: 'bash', argumentsDelta: argumentsJson },
      { type: 'block-end', index: 0, block: { type: 'tool-call', id: 'pwsh-native-fail', name: 'bash', arguments: argumentsJson } },
      { type: 'finish', reason: { kind: 'tool-calls' } },
    ],
    [
      { type: 'block-start', index: 0, blockType: 'text' },
      { type: 'text-delta', index: 0, text: DONE },
      { type: 'block-end', index: 0, block: { type: 'text', text: DONE } },
      { type: 'finish', reason: { kind: 'stop' } },
    ],
  ]
  return attempts.flatMap((chunks, attempt) => chunks.map(chunk => JSON.stringify({
    provider: 'recorded', model: 'recorded', requestId: `pwsh-native-${attempt}`, chunk,
  }))).join('\n')
}

const pwshTest = HAS_PWSH ? test : test.skip

pwshTest('pwsh-terminal runs PowerShell through the native bash terminal card', async () => {
  expect((await readFile(SEED, 'utf8')).includes(PROMPT)).toBe(true)
  expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['seed.jsonl', 'terminal-card.expected.md'])
  expect(await readFile(TERMINAL_EXPECTED, 'utf8')).toContain('exit code 1')

  const harness = await RustWebHarness.launch({
    name: 'pwsh-terminal-web-e2e',
    locale: 'en-US',
    replayRecording: replayRecording(),
  })
  try {
    const settled = harness.whenTurnSettled()
    await harness.page.locator('textarea:enabled').last().fill(PROMPT)
    await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
    const sessionId = await settled
    await harness.page.getByText(DONE, { exact: true }).waitFor({ timeout: 15_000 })

    const eventLog = join(harness.dataDir, `session-${Buffer.from(sessionId).toString('hex')}.jsonl`)
    await waitUntil(() => readFile(eventLog, 'utf8'), document => (
      document.includes('"name":"bash"')
      && document.includes('"exitCode":1')
      && document.includes('[exit code: 1]')
    ))
    const call = harness.page.locator('[data-sample="bash"]').first()
    await call.waitFor({ timeout: 15_000 })
    const disclosure = call
    if (await disclosure.getAttribute('aria-expanded') !== 'true') await disclosure.click()
    const card = call.locator('xpath=..').locator('[data-terminal]').first()
    await card.waitFor({ timeout: 15_000 })
    const text = await card.textContent()
    expect(text).toContain('exit code 1')
    expect(text).toContain('Get-Item')
    expect(await card.getByRole('button', { name: 'Copy', exact: true }).count()).toBe(1)
    expect(await captureStableAria(harness.page, '[data-terminal]')).toContain('exit code 1')
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
