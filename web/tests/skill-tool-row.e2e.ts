import { expect, test } from 'bun:test'
import { openSessionByMarker, RustWebHarness } from './support'

const SESSION = 'skill-tool-row-web-e2e'
const PROMPT = 'Load the snapshot-skill skill with the skill tool, then reply DONE.'
const INSTRUCTIONS = 'Follow these snapshot-only instructions.'
const OUTPUT = `<skill_content name="snapshot-skill">\n<skill_resources>\nBase directory for this skill: {{cwd}}/.dsh/skills/snapshot-skill\nResolve relative paths mentioned by this skill against the base directory before using them. Load referenced resources only as needed.\n</skill_resources>\n\n<skill_instructions>\n${INSTRUCTIONS}\nResolve referenced resources relative to this skill directory.\n</skill_instructions>\n</skill_content>`
interface ToolResultMessage {
  content?: Array<{ content?: Array<{ text?: string }> }>
}

interface HistoryEvent {
  event: { type: string; data: { message?: ToolResultMessage } }
}


function recording(): string {
  const time = 1_785_000_000_000
  const callId = 'snapshot-skill-call'
  const rows = [
    { type: 'session', version: 0, id: '{{sessionId}}', createdAt: time, cwd: '{{cwd}}' },
    { type: 'turn/start', time, seq: 0, data: { turn: 1 } },
    { type: 'user/message', time, seq: 1, data: { id: 'skill-user', role: 'user', content: [{ type: 'text', text: PROMPT }], source: { kind: 'user' } }, surfaceOp: 'append' },
    { type: 'step/start', time, seq: 2, data: { turn: 1, step: 1 } },
    { type: 'assistant/message', time, seq: 3, data: { turn: 1, step: 1, message: { id: 'skill-call', role: 'assistant', content: [{ type: 'tool-call', id: callId, name: 'skill', arguments: JSON.stringify({ name: 'snapshot-skill' }) }], source: { kind: 'model', provider: 'fixture', model: 'fixture' } } }, surfaceOp: 'append' },
    { type: 'tool/call', time, seq: 4, data: { turn: 1, step: 1, callId, name: 'skill', arguments: JSON.stringify({ name: 'snapshot-skill' }) } },
    { type: 'tool/result', time, seq: 5, data: { turn: 1, step: 1, message: { id: 'skill-result', role: 'user', content: [{ type: 'tool-result', toolCallId: callId, content: [{ type: 'text', text: OUTPUT }], isError: false }], source: { kind: 'tool', callId } }, sourceEventSeqs: [4] }, surfaceOp: 'append' },
    { type: 'step/end', time, seq: 6, data: { turn: 1, step: 1 } },
    { type: 'step/start', time, seq: 7, data: { turn: 1, step: 2 } },
    { type: 'assistant/message', time, seq: 8, data: { turn: 1, step: 2, message: { id: 'skill-done', role: 'assistant', content: [{ type: 'text', text: 'DONE' }], source: { kind: 'model', provider: 'fixture', model: 'fixture' } } }, surfaceOp: 'append' },
    { type: 'step/end', time, seq: 9, data: { turn: 1, step: 2 } },
    { type: 'turn/end', time, seq: 10, data: { turn: 1, reason: { kind: 'completed' } } },
  ]
  return rows.map(row => JSON.stringify(row)).join('\n')
}

test('the dedicated Skill row expands to its exact recorded instruction block', async () => {
  const harness = await RustWebHarness.launch({
    name: SESSION,
    beforeStart: candidate => candidate.seedSession(SESSION, recording()),
  })
  try {
    await openSessionByMarker(harness, PROMPT, 'DONE')
    const call = harness.page.locator('[data-tool="skill"]').first()
    await call.waitFor({ timeout: 15_000 })
    const row = call.getByRole('button', { name: 'Skill snapshot-skill' })
    expect(await row.getAttribute('aria-expanded')).toBe('false')
    expect(await call.getByText('snapshot-skill', { exact: true }).count()).toBe(1)

    await row.click()
    expect(await row.getAttribute('aria-expanded')).toBe('true')
    await call.getByText('Instructions', { exact: true }).waitFor({ timeout: 15_000 })
    const output = call.locator('pre')
    await output.waitFor({ timeout: 15_000 })
    const durableOutput = OUTPUT.replaceAll('{{cwd}}', harness.workspace)
    expect(await output.textContent()).toBe(durableOutput)

    const history = await harness.rpc<{ events: HistoryEvent[] }>('session.history', { sessionId: SESSION, maxMessages: 100 })
    expect(history.ok).toBe(true)
    const result = history.value?.events.find(({ event }) => event.type === 'tool/result')
    expect(result?.event.data.message?.content[0]?.content?.[0]?.text).toBe(durableOutput)
    harness.assertClean()
  } finally {
    await harness.close()
  }
})
