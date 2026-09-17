import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

const PROMPT = 'Run a PowerShell command that fails, then stop.'
const DONE = 'PWSH_NATIVE_TERMINAL_DONE'

function replayRecording(): string {
  const argumentsJson = JSON.stringify({
    command: process.platform === 'win32'
      ? "Get-Item -LiteralPath 'missing.txt' -ErrorAction Stop"
      : "pwsh -NoLogo -NoProfile -NonInteractive -Command \"Get-Item -LiteralPath 'missing.txt' -ErrorAction Stop\"",
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


test('pwsh-terminal runs PowerShell through the native bash terminal card', async () => {

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
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
