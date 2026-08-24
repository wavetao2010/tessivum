import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises'
import { dirname, join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, RustWebHarness, stableAria, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/goal-multi-turn-actions')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const OVERRIDE = join(SNAPSHOT_DIR, 'replay.override.json')
const UI_EXPECTED = join(SNAPSHOT_DIR, 'ui.expected.md')

const PROMPT = '做两个turn，每个turn输出随机一个包的文件结构。注意你做完一个turn之后，直接输出内容，停止，我们的系统会帮你再开一个turn，你看着做一个类似的'
const COMMAND = `/goal ${PROMPT}`

const PACKAGE_FILES: Readonly<Record<string, string>> = {
  'packages/client/ui-conversation/README.md': '# UI conversation\n',
  'packages/client/ui-conversation/package.json': '{"name":"@deepseek-ai/dsh-client-ui-conversation"}\n',
  'packages/client/ui-conversation/src/client.ts': 'export {}\n',
  'packages/client/ui-conversation/tests/chat-view.client.spec.tsx': 'export {}\n',
  'packages/context/session-reference/README.md': '# Session reference\n',
  'packages/context/session-reference/package.json': '{"name":"@deepseek-ai/dsh-session-reference"}\n',
  'packages/context/session-reference/src/index.ts': 'export {}\n',
  'packages/context/session-reference/src/uri.ts': 'export {}\n',
  'packages/context/session-reference/tests/session-reference.spec.ts': 'export {}\n',
  'packages/llm/token-meter/README.md': '# Token meter\n',
  'packages/llm/token-meter/package.json': '{"name":"@deepseek-ai/dsh-token-meter"}\n',
  'packages/llm/token-meter/src/index.ts': 'export {}\n',
  'packages/llm/token-meter/tests/token-meter.spec.ts': 'export {}\n',
  'packages/skill/skill-filesystem/README.md': '# Local skill provider\n',
  'packages/skill/skill-filesystem/package.json': '{"name":"@deepseek-ai/dsh-skill-filesystem"}\n',
  'packages/skill/skill-filesystem/src/index.ts': 'export {}\n',
  'packages/skill/skill-filesystem/src/invariant.ts': 'export {}\n',
  'packages/skill/skill-filesystem/tests/skill-filesystem.spec.ts': 'export {}\n',
}

type JsonRecord = Record<string, unknown>
type SessionEvent = { type: string; data: JsonRecord }

function record(value: unknown): value is JsonRecord {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function stringField(value: unknown, field: string): string | undefined {
  const item = record(value) ? value[field] : undefined
  return typeof item === 'string' ? item : undefined
}

function numberField(value: unknown, field: string): number | undefined {
  const item = record(value) ? value[field] : undefined
  return typeof item === 'number' ? item : undefined
}

async function seedPackageInventory(workspace: string): Promise<void> {
  await Promise.all(Object.entries(PACKAGE_FILES).map(async ([relative, content]) => {
    const path = join(workspace, relative)
    await mkdir(dirname(path), { recursive: true })
    await writeFile(path, content)
  }))
}

function sessionEvents(document: string): SessionEvent[] {
  return document.trim().split('\n').slice(1).map(line => {
    const value = JSON.parse(line) as unknown
    if (!record(value) || typeof value.type !== 'string' || !record(value.data)) {
      throw new Error('invalid durable session event')
    }
    return { type: value.type, data: value.data }
  })
}

async function events(harness: RustWebHarness, sessionId: string): Promise<SessionEvent[]> {
  const path = join(harness.dataDir, `session-${Buffer.from(sessionId).toString('hex')}.jsonl`)
  return sessionEvents(await readFile(path, 'utf8'))
}

function goalRounds(events: readonly SessionEvent[]): number[] {
  return events.flatMap(event => event.type === 'user/message' && stringField(event.data.source, 'kind') === 'goal'
    ? [numberField(event.data.source, 'round')].filter((round): round is number => round !== undefined)
    : [])
}

function createdObjectives(events: readonly SessionEvent[]): string[] {
  return events.flatMap(event => event.type === 'goal/change' && stringField(event.data, 'operation') === 'create'
    ? [stringField(event.data.goal, 'objective')].filter((objective): objective is string => objective !== undefined)
    : [])
}

function goalPhase(events: readonly SessionEvent[]): string | undefined {
  return stringField(events.findLast(event => event.type === 'goal/change')?.data.goal, 'phase')
}

function endedTurns(events: readonly SessionEvent[]): number[] {
  return events.flatMap(event => event.type === 'turn/end'
    ? [numberField(event.data, 'turn')].filter((turn): turn is number => turn !== undefined)
    : [])
}

function stableGoalAria(snapshot: string): string {
  return stableAria(snapshot.replace(/goal-[0-9a-f-]{36}/g, 'goal-{{uuid}}'))
}

test('goal keeps actions on both completed durable turns after reload', async () => {
  const fixtureEvents = sessionEvents(await readFile(FIXTURE, 'utf8'))
  expect(createdObjectives(fixtureEvents)).toEqual([PROMPT])
  expect(goalRounds(fixtureEvents)).toEqual([1, 2])
  const harness = await RustWebHarness.launch({
    name: 'goal-multi-turn-actions',
    locale: 'en-US',
    replayFixture: FIXTURE,
    replayOverride: OVERRIDE,
    beforeStart: candidate => seedPackageInventory(candidate.workspace),
  })
  try {
    const [{ sessionId }] = await harness.sessions()
    const composer = harness.page.locator('textarea').first()
    await composer.waitFor({ timeout: 10_000 })
    await composer.fill(COMMAND)
    await composer.press('Enter')

    const settled = await waitUntil(
      () => events(harness, sessionId),
      value => value.filter(event => event.type === 'turn/end').length === 2,
      120_000,
    )
    expect(createdObjectives(settled)).toEqual([PROMPT])
    expect(goalRounds(settled)).toEqual([1, 2])
    expect(endedTurns(settled)).toEqual([1, 2])
    expect(goalPhase(settled)).toBe('complete')
    const bashCallIds = new Set(settled.flatMap(event => event.type === 'tool/call'
      && event.data.name === 'bash' && typeof event.data.callId === 'string' ? [event.data.callId] : []))
    const bashResults = settled.filter(event => event.type === 'tool/result'
      && bashCallIds.has(stringField(record(event.data.message) ? event.data.message.source : undefined, 'callId') ?? ''))
    expect(bashResults.length).toBeGreaterThan(0)
    const bashErrors = bashResults.map(event => {
      const message = record(event.data.message) ? event.data.message : undefined
      const block = Array.isArray(message?.content) ? message.content[0] : undefined
      return record(block) ? block.isError : undefined
    })
    expect(bashErrors).not.toContain(undefined)
    expect(bashErrors.filter(Boolean)).toHaveLength(1)

    await harness.page.reload({ waitUntil: 'load' })
    await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
    await harness.page.getByText('Turn 2 / 2', { exact: false }).waitFor({ timeout: 15_000 })
    const restored = await events(harness, sessionId)
    expect(createdObjectives(restored)).toEqual([PROMPT])
    expect(goalRounds(restored)).toEqual([1, 2])
    expect(goalPhase(restored)).toBe('complete')

    const branchButtons = harness.page.getByRole('button', { name: 'Branch into a new conversation' })
    await waitUntil(() => branchButtons.count(), count => count === 2)
    expect(await branchButtons.evaluateAll(buttons => buttons.map(button => button.getAttribute('aria-disabled'))))
      .toEqual([null, null])
    await branchButtons.last().focus()
    expect(stableGoalAria(await captureStableAria(harness.page, '[class*="centerCol"]')))
      .toBe((await Bun.file(UI_EXPECTED).text()).trim())
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 140_000)

test('goal multi-turn fixture inventory remains closed', async () => {
  expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual([
    'replay.override.json', 'session.jsonl', 'ui.expected.md',
  ].sort())
})
