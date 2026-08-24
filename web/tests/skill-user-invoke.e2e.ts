import { mkdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness } from './support'

const SKILL = 'user-invoke-demo'
const ARGUMENTS = 'and confirm the fixture wiring'
const REPLY = 'USER_INVOKE_REPLY acknowledged; following the injected skill.'

interface InstructionMessage {
  content?: { text?: string }[]
  source?: { kind?: string; name?: string; plugin?: string; form?: string }
}

interface HistoryEvent {
  event: { type: string; data: InstructionMessage }
}

function replayRecording(): string {
  const chunks = [
    { type: 'block-start', index: 0, blockType: 'text' },
    { type: 'text-delta', index: 0, text: REPLY },
    { type: 'block-end', index: 0, block: { type: 'text', text: REPLY } },
    { type: 'usage', usage: { inputTokens: 256, outputTokens: 16 } },
    { type: 'finish', reason: { kind: 'stop' } },
  ]
  return [
    { type: 'session', version: 0, id: 'skill-user-invoke-replay', createdAt: 0, cwd: '/workspace' },
    ...chunks.map((chunk, seq) => ({ type: 'assistant/chunk', seq, time: 0, data: { turn: 1, step: 1, chunk } })),
  ].map(row => JSON.stringify(row)).join('\n')
}

test('a user-only slash invocation persists its gesture and instruction context before replaying', async () => {
  const harness = await RustWebHarness.launch({
    name: 'skill-user-invoke-web-e2e',
    replayRecording: replayRecording(),
    beforeStart: async candidate => {
      const directory = join(candidate.workspace, '.agents', 'skills', SKILL)
      await mkdir(directory, { recursive: true })
      await writeFile(join(directory, 'SKILL.md'), [
        '---', `name: ${SKILL}`, 'description: Prove user-explicit invocation of a model-hidden skill',
        'disable-model-invocation: true', '---', '', 'Reply with the fixture acknowledgement line.', '',
      ].join('\n'))
    },
  })
  try {
    const composer = harness.page.locator('textarea:enabled').last()
    await composer.fill(`/${SKILL}`)
    const menu = harness.page.getByRole('listbox', { name: 'Trigger suggestions' })
    await menu.getByRole('option', { name: new RegExp(SKILL) }).waitFor({ timeout: 15_000 })

    const settled = harness.whenTurnSettled()
    await composer.fill(`/${SKILL} ${ARGUMENTS}`)
    await composer.press('Enter')

    const bubble = harness.page.locator('[data-ref-chip="skill"]').first()
    await bubble.waitFor({ timeout: 15_000 })
    expect(await bubble.textContent()).toBe(`/${SKILL}`)

    const injection = harness.page.getByRole('button', { name: `Context injection ${SKILL}` })
    await injection.waitFor({ timeout: 15_000 })
    await injection.click()
    const body = harness.page.locator('[data-context-injection-body]').filter({ hasText: `<skill_content name="${SKILL}">` })
    await body.waitFor({ timeout: 15_000 })
    expect(await body.textContent()).toContain('Reply with the fixture acknowledgement line.')
    expect(await body.textContent()).not.toContain(ARGUMENTS)
    await injection.click()

    const sessionId = await settled
    await harness.page.getByText('USER_INVOKE_REPLY', { exact: false }).waitFor({ timeout: 15_000 })
    const history = await harness.rpc<{ events: HistoryEvent[] }>('session.history', { sessionId, maxMessages: 100 })
    expect(history.ok).toBe(true)
    const events = history.value?.events ?? []
    const gesture = events.find(({ event }) =>
      event.type === 'user/message' && event.data.source?.kind === 'user')
    expect(gesture?.event.data.content?.[0]?.text).toBe(`/${SKILL} ${ARGUMENTS}`)
    const injected = events.find(({ event }) =>
      event.type === 'user/message'
      && event.data.source?.kind === 'skill-invocation'
      && event.data.source?.name === SKILL
      && event.data.source?.form === 'instructions')
    const content = injected?.event.data.content?.[0]?.text
    expect(content).toContain(`<skill_content name="${SKILL}">`)
    expect(content).not.toContain(ARGUMENTS)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
