import { readFile, readdir, realpath } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/permission-policy-context')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')

const PROMPTS = [
  'Can you create or edit a normal file right now under the current policy? Answer directly in one sentence. Do not call a tool just to discover the policy.',
  'Does the DSH file sandbox currently restrict file operations? Answer directly in one sentence. Do not call tools.',
  'Reply with exactly WORKSPACE_POLICY_SEEN. Do not call tools.',
  'Create the relative path policy-neutral.txt in the current workspace containing exactly POLICY_NEUTRAL_OK, verify its contents, then report completion.',
] as const
const PRESET_LABELS = ['Read Only', 'Full access', 'Workspace Write'] as const

type ObjectValue = Record<string, unknown>
type Event = { type: string; data: ObjectValue }

function object(value: unknown): ObjectValue | undefined {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as ObjectValue
    : undefined
}

function parseEvent(line: string): Event {
  const root = object(JSON.parse(line) as unknown)
  if (root === undefined || typeof root.type !== 'string' || object(root.data) === undefined) {
    throw new Error('fixture event is malformed')
  }
  return { type: root.type, data: root.data as ObjectValue }
}

function textBlocks(value: unknown): string[] {
  return Array.isArray(value) ? value.flatMap((block) => {
    const item = object(block)
    return item?.type === 'text' && typeof item.text === 'string' ? [item.text] : []
  }) : []
}

async function events(harness: RustWebHarness, sessionId: string): Promise<Event[]> {
  const file = join(harness.dataDir, `session-${Buffer.from(sessionId).toString('hex')}.jsonl`)
  return (await readFile(file, 'utf8')).trim().split('\n').slice(1).map(parseEvent)
}

function fixtureUserPrompts(recording: string): string[] {
  return recording.trim().split('\n').slice(1).flatMap((line) => {
    const event = parseEvent(line)
    return event.type === 'user/message' && object(event.data.source)?.kind === 'user'
      ? textBlocks(event.data.content) : []
  })
}

function requestSystems(log: readonly Event[]): string[] {
  return log.flatMap((event) => {
    const system = object(event.data.header)?.system
    return event.type === 'request/header' && typeof system === 'string' ? [system] : []
  })
}

function runtimeContexts(log: readonly Event[]): string[] {
  return log.flatMap((event) => {
    const source = object(event.data.source)
    return event.type === 'user/message'
      && source?.kind === 'plugin'
      && source.plugin === '@deepseek-ai/dsh-system-prompt'
      ? textBlocks(event.data.content) : []
  })
}

function assistantTexts(log: readonly Event[]): string[] {
  return log.flatMap((event) => event.type === 'assistant/message'
    ? textBlocks(object(event.data.message)?.content).map(text => text.replaceAll('**', '')) : [])
}

function callArgs(call: Event): ObjectValue {
  const raw = call.data.arguments
  const parsed = typeof raw === 'string' ? object(JSON.parse(raw) as unknown) : undefined
  if (parsed === undefined) throw new Error('tool call arguments are malformed')
  return parsed
}

test('switches read-only, danger-full-access, and workspace-write through the real GUI command path', async () => {
  const harness = await RustWebHarness.launch({
    name: 'permission-policy-context-web-e2e', locale: 'en-US', replayFixture: FIXTURE,
  })
  try {
    expect(fixtureUserPrompts(await readFile(FIXTURE, 'utf8'))).toEqual(PROMPTS)
    const input = harness.page.locator('textarea').first()
    let sessionId: string | undefined
    for (const [index, preset] of ['read-only', 'danger-full-access', 'workspace-write'].entries()) {
      await input.fill(`/permission ${preset}`)
      await input.press('Enter')
      await harness.page.getByRole('button', { name: `Access mode, current: ${PRESET_LABELS[index]}` }).waitFor({ timeout: 10_000 })
      const settled = harness.whenTurnSettled()
      await input.fill(PROMPTS[index]!)
      await input.press('Enter')
      sessionId = await settled
      await expect(input.isEnabled()).resolves.toBe(true)
    }

    await input.fill('/permission read-only')
    await input.press('Enter')
    await harness.page.getByRole('button', { name: 'Access mode, current: Read Only' }).waitFor({ timeout: 10_000 })
    const settled = harness.whenTurnSettled()
    await input.fill(PROMPTS[3])
    await input.press('Enter')
    const approval = harness.page.locator('[data-approval-key]')
    await approval.waitFor({ timeout: 60_000 })
    await approval.getByRole('button', { name: 'Allow once' }).click()
    sessionId = await settled
    if (sessionId === undefined) throw new Error('permission-policy scenario completed no model turn')

    const log = await events(harness, sessionId)
    const systems = requestSystems(log)
    expect(systems).toHaveLength(1)
    expect(systems[0]).not.toContain('Current DSH file policy:')
    expect(systems[0]).not.toContain('Approval policy:')
    expect(systems[0]).not.toContain('Approval prompts are disabled in this session')

    const contexts = runtimeContexts(log)
    expect(contexts).toHaveLength(4)
    expect(contexts[0]).toContain('Current DSH file policy: read-only. Any available operation enforced by the DSH file sandbox cannot modify files in the standing mode.')
    expect(contexts[0]).toContain('Do not refuse a required modification from this policy alone')
    expect(contexts[0]).toContain('Approval policy: ask.')
    expect(contexts[1]).toContain('Current DSH file policy: danger-full-access. The DSH file sandbox does not restrict file modifications by available operations.')
    expect(contexts[1]).toContain('Approval prompts are disabled in this session')
    const workspace = await realpath(harness.workspace)
    expect(contexts[2]).toContain(`Current DSH file policy: workspace-write. Any available operation enforced by the DSH file sandbox may modify files under the session workspace: ${JSON.stringify(workspace)}. Some platform temporary areas may also be writable.`)
    expect(contexts[2]).toContain('Approval policy: ask.')
    expect(contexts[2]).not.toContain('Approval prompts are disabled in this session')
    expect(contexts[3]).toContain('Current DSH file policy: read-only.')

    const answers = assistantTexts(log)
    expect(answers.length).toBeGreaterThanOrEqual(4)
    expect(answers[0]).toMatch(/read-only.*(?:denied|cannot modify|cannot create or edit)/i)
    expect(answers[1]).toMatch(/does not restrict.*(?:file operations|(?:write\/edit tools|write and edit tools).*one-shot bash commands)/i)
    expect(answers[2]).toBe('WORKSPACE_POLICY_SEEN')
    const calls = log.filter(event => event.type === 'tool/call')
    expect(calls.every(call => call.data.turn === 4)).toBe(true)
    const writes = calls.filter(call => call.data.name === 'write')
    expect(writes).toHaveLength(2)
    const initialWrite = callArgs(writes[0]!)
    const approvedWrite = callArgs(writes[1]!)
    expect(initialWrite).toMatchObject({ file_path: 'policy-neutral.txt', content: 'POLICY_NEUTRAL_OK' })
    expect(initialWrite.sandbox_permissions).toBeUndefined()
    expect(approvedWrite).toMatchObject({
      file_path: 'policy-neutral.txt',
      content: 'POLICY_NEUTRAL_OK',
      sandbox_permissions: 'workspace-write',
    })
    expect(typeof approvedWrite.justification === 'string' && approvedWrite.justification.trim() !== '').toBe(true)
    const denied = log.find(event => event.type === 'tool/result'
      && object(event.data.meta)?.code === 'SANDBOX_WRITE_DENIED')
    expect(denied).toBeDefined()
    expect(log.find(event => event.type === 'approval/asked')?.data.callId).toBe(writes[1]?.data.callId)
    expect(log.find(event => event.type === 'approval/decided')?.data.outcome).toBe('allowed-once')
    const approvedResult = log.find(event => event.type === 'tool/result'
      && object(event.data.message)?.source
      && object(object(event.data.message)?.source)?.callId === writes[1]?.data.callId)
    expect(JSON.stringify(approvedResult)).toContain('Created file')
    expect(JSON.stringify(approvedResult)).toContain('"isError":false')
    const reads = calls.filter(call => call.data.name === 'read')
    expect(reads).toHaveLength(1)
    expect(callArgs(reads[0]!)).toEqual({ file_path: 'policy-neutral.txt' })
    expect(JSON.stringify(log.find(event => event.type === 'tool/result'
      && object(object(event.data.message)?.source)?.callId === reads[0]?.data.callId))).toContain('POLICY_NEUTRAL_OK')
    expect(answers.at(-1)).toContain('POLICY_NEUTRAL_OK')
    expect(await readFile(join(workspace, 'policy-neutral.txt'), 'utf8')).toBe('POLICY_NEUTRAL_OK')
    expect(await readdir(SNAPSHOT_DIR)).toEqual(['session.jsonl'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 240_000)
