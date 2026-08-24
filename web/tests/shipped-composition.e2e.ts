import { writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness } from './support'

const REPLY = 'SHIPPED_COMPOSITION_READY'
const FILE_REFERENCE_GUIDANCE = 'When you successfully create or modify files, mention the primary outputs in your final response.'
const WORKSPACE_INSTRUCTION = 'SHIPPED_WORKSPACE_CONTEXT must reach the model request.'

const EXPECTED_TOOLS = [
  'ask_user_question',
  'bash',
  'create_goal',
  'edit',
  'exit_plan_mode',
  'get_goal',
  'glob',
  'grep',
  'interrupt_agent',
  'jobs.kill',
  'jobs.list',
  'jobs.read',
  'jobs.wait',
  'list_agents',
  'ralph',
  'read',
  'read_image',
  'send_message',
  'schedule_create',
  'schedule_delete',
  'schedule_list',
  'skill',
  'subagent',
  'subagent_fork',
  'todo_write',
  'update_goal',
  'web_search',
  'workflow',
  'write',
].sort()

interface RequestHeader {
  system?: string
  tools?: { name: string }[]
}

interface HistoryEvent {
  event: { type: string; data: { header?: RequestHeader } }
}

function replayRecording(): string {
  const bash = JSON.stringify({
    command: 'printf SHIPPED_BACKGROUND_OK',
    description: 'Verify the shipped background-job registry',
    run_in_background: true,
  })
  const attempts = [
    [
      { type: 'block-start', index: 0, blockType: 'tool-call' },
      { type: 'tool-call-delta', index: 0, id: 'shipped-background', name: 'bash', argumentsDelta: bash },
      { type: 'block-end', index: 0, block: { type: 'tool-call', id: 'shipped-background', name: 'bash', arguments: bash } },
      { type: 'finish', reason: { kind: 'tool-calls' } },
    ],
    [
      { type: 'block-start', index: 0, blockType: 'text' },
      { type: 'text-delta', index: 0, text: `${REPLY} {{fromRequest:(SHIPPED_WORKSPACE_CONTEXT must reach the model request\\.)}}` },
      { type: 'block-end', index: 0, block: { type: 'text', text: `${REPLY} {{fromRequest:(SHIPPED_WORKSPACE_CONTEXT must reach the model request\\.)}}` } },
      { type: 'finish', reason: { kind: 'stop' } },
    ],
  ]
  let seq = 0
  return [
    { type: 'session', version: 0, id: 'shipped-composition-replay', createdAt: 0, cwd: '/workspace' },
    ...attempts.flatMap((chunks, step) => chunks.map(chunk => ({ type: 'assistant/chunk', seq: seq++, time: 0, data: { turn: 1, step: step + 1, chunk } }))),
  ].map(row => JSON.stringify(row)).join('\n')
}

test('the shipped Web composition exposes its full model catalog, guidance, policy, and job registry', async () => {
  const harness = await RustWebHarness.launch({
    name: 'shipped-composition-web-e2e',
    locale: 'en-US',
    replayRecording: replayRecording(),
    beforeStart: candidate => writeFile(join(candidate.workspace, 'AGENTS.md'), `${WORKSPACE_INSTRUCTION}\n`),
  })
  try {
    expect(await harness.page.getByRole('button', { name: /Access mode/ }).textContent()).toContain('Workspace Write')
    const settled = harness.whenTurnSettled()
    await harness.page.locator('textarea:enabled').last().fill('Inspect the shipped Web composition.')
    await harness.page.getByRole('button', { name: 'Send message', exact: true }).click()
    const sessionId = await settled
    await harness.page.getByText(REPLY, { exact: false }).waitFor({ timeout: 15_000 })

    const history = await harness.rpc<{ events: HistoryEvent[] }>('session.history', { sessionId, maxMessages: 100 })
    expect(history.ok).toBe(true)
    const header = history.value?.events.find(({ event }) => event.type === 'request/header')?.event.data.header
    expect(header?.system).toContain(FILE_REFERENCE_GUIDANCE)
    expect(header?.system).toContain('Tessivum Web GUI')
    const names = (header?.tools ?? []).map(tool => tool.name).sort()
    expect(names).toEqual(EXPECTED_TOOLS)
    expect(JSON.stringify(history.value)).toContain('Started background job')
    expect(JSON.stringify(history.value)).toContain('SHIPPED_BACKGROUND_OK')
    await harness.page.getByText(WORKSPACE_INSTRUCTION, { exact: false }).waitFor({ timeout: 15_000 })
    const events = history.value?.events.map(({ event }) => event) ?? []
    expect(events.find(event => event.type === 'permission/preset')?.data).toEqual({ preset: 'workspace-write' })
    expect(events.find(event => event.type === 'sandbox/mode')?.data).toEqual({ mode: 'workspace-write' })
    expect(events.find(event => event.type === 'approval/policy')?.data).toEqual({ policy: 'ask' })
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
