import { expect } from 'bun:test'
import { chmod, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { delimiter, dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { chromium, type Browser, type Page } from 'playwright-core'

const HERE = dirname(fileURLToPath(import.meta.url))
export const CRATE_ROOT = join(HERE, '../..')
export const UPSTREAM_ROOT = process.env.TESSIVUM_DEEPSEEK_SOURCE ?? join(CRATE_ROOT, '../upstream/deepseek-harness')
const SHIPPED_PRESETS = join(UPSTREAM_ROOT, 'apps/cli/config/agent-presets')
export const UPSTREAM_TESTS = join(UPSTREAM_ROOT, 'apps/web/tests')
const CARGO = process.env.CARGO_BIN ?? 'cargo'
let build: Promise<void> | undefined

export interface RpcResult<T> {
  ok: boolean
  value?: T
  error?: { code: string; message: string; data?: unknown }
}

export interface SessionListItem {
  sessionId: string
  cwd?: string
  updatedAt: number
  running: boolean
  blank: boolean
  eventCount: number
}

export interface RustWebOptions {
  name: string
  toolsMode?: 'native' | 'code'
  locale?: string
  remoteAuthority?: string
  timeZoneId?: string
  showWelcomeNotice?: boolean
  preserveCredentialOnboarding?: boolean
  replayFixture?: string
  deepSeekSearch?: { baseURL: string; apiKeyEnv: string; apiKey?: string }
  replayRecording?: string | (() => string)
  replayOverride?: string
  env?: Record<string, string>
  clientPackageRoots?: string[]
  beforeStart?: (harness: RustWebHarness) => Promise<void>
  beforePage?: (harness: RustWebHarness) => Promise<void>
  viewport?: { width: number; height: number }
}

async function buildBinary(): Promise<void> {
  if (build === undefined) {
    build = (async () => {
      const child = Bun.spawn([
        CARGO, 'build', '--quiet', '--manifest-path', join(CRATE_ROOT, 'Cargo.toml'), '--bin', 'tessivum',
      ], { cwd: CRATE_ROOT, stdout: 'inherit', stderr: 'inherit' })
      expect(await child.exited).toBe(0)
    })()
  }
  await build
}

async function freePort(): Promise<number> {
  const probe = createServer()
  await new Promise<void>((resolve, reject) => {
    probe.once('error', reject)
    probe.listen(0, '127.0.0.1', resolve)
  })
  const address = probe.address()
  if (address === null || typeof address === 'string') throw new Error('failed to reserve a TCP port')
  await new Promise<void>((resolve, reject) => probe.close(error => error === undefined ? resolve() : reject(error)))
  return address.port
}

async function waitForServer(url: string): Promise<void> {
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    try {
      if ((await fetch(url)).ok) return
    } catch {}
    await Bun.sleep(50)
  }
  throw new Error(`native Tessivum server did not become ready at ${url}`)
}

export class RustWebHarness {
  readonly root: string
  readonly workspace: string
  readonly dataDir: string
  readonly baseUrl: string
  readonly pageErrors: string[] = []
  readonly warnings: string[] = []
  readonly httpErrors: string[] = []
  browser!: Browser
  page!: Page
  private server!: Bun.Subprocess

  private constructor(root: string, workspace: string, port: number) {
    this.root = root
    this.workspace = workspace
    this.dataDir = join(workspace, '.tessivum')
    this.baseUrl = `http://127.0.0.1:${port}`
  }

  static async launch(options: RustWebOptions): Promise<RustWebHarness> {
    await buildBinary()
    const root = await realpath(await mkdtemp(join(tmpdir(), `tessivum-${options.name}-`)))
    const workspace = join(root, 'workspace')
    await mkdir(workspace)
    const harness = new RustWebHarness(root, workspace, await freePort())
    try {
      await options.beforeStart?.(harness)
      const env: Record<string, string> = {
        ...process.env as Record<string, string>,
        DEEPSEEK_API_KEY: 'test',
        TESSIVUM_AGENT_PRESET_ROOT: SHIPPED_PRESETS,
        ...options.env,
        TESSIVUM_WEB_ADDR: harness.baseUrl.slice('http://'.length),
      }
      if (options.clientPackageRoots !== undefined) {
        env.TESSIVUM_CLIENT_PACKAGES = options.clientPackageRoots.join(delimiter)
      }
      if (options.remoteAuthority !== undefined) {
        env.TESSIVUM_WEB_TRUSTED_AUTHORITIES = `${options.remoteAuthority}:${new URL(harness.baseUrl).port}`
      }
      if (options.toolsMode !== undefined) env.TESSIVUM_TOOLS_MODE = options.toolsMode
      if (options.replayRecording !== undefined) {
        const replay = join(root, 'replay.jsonl')
        const recording = typeof options.replayRecording === 'function'
          ? options.replayRecording()
          : options.replayRecording
        await writeFile(replay, recording)
        env.TESSIVUM_REPLAY_FILE = replay
      } else if (options.replayFixture !== undefined) {
        env.TESSIVUM_REPLAY_FILE = options.replayFixture
      }
      if (options.replayRecording !== undefined || options.replayFixture !== undefined) {
        env.TESSIVUM_REPLAY_CONTEXT_WINDOW = '128000'
      }
      if (options.deepSeekSearch !== undefined) {
        env.DEEPSEEK_SEARCH_BASE_URL = options.deepSeekSearch.baseURL
        env.DEEPSEEK_SEARCH_API_KEY_ENV = options.deepSeekSearch.apiKeyEnv
      }
      if (options.replayOverride !== undefined) env.TESSIVUM_REPLAY_OVERRIDE_FILE = options.replayOverride
      harness.server = Bun.spawn([join(CRATE_ROOT, 'target/debug/tessivum'), 'web'], {
        cwd: workspace,
        env,
        stdout: 'inherit',
        stderr: 'inherit',
      })
      await waitForServer(harness.baseUrl)
      if (options.deepSeekSearch?.apiKey !== undefined) {
        const credential = await harness.rpc('credentials.set', {
          ref: options.deepSeekSearch.apiKeyEnv,
          value: options.deepSeekSearch.apiKey,
        })
        if (!credential.ok) throw new Error(`credentials.set failed: ${JSON.stringify(credential.error)}`)
      }
      await options.beforePage?.(harness)
      harness.browser = await chromium.launch(process.env.TESSIVUM_CHROMIUM === undefined
        ? { channel: 'chrome' }
        : { executablePath: process.env.TESSIVUM_CHROMIUM })
      harness.page = await harness.browser.newPage({
        viewport: options.viewport ?? { width: 1680, height: 1000 },
        locale: options.locale ?? 'en-US',
        timezoneId: options.timeZoneId,
      })
      harness.page.on('pageerror', error => harness.pageErrors.push(error.message))
      harness.page.on('console', message => {
        if (message.type() === 'warning' || message.type() === 'error') harness.warnings.push(message.text())
      })
      harness.page.on('response', async response => {
        if (response.status() >= 400) {
          const body = await response.text().catch(() => '')
          harness.httpErrors.push(`${response.status()} ${response.url()} ${response.request().postData() ?? ''} ${body}`)
        }
      })
      const pageUrl = options.remoteAuthority === undefined
        ? harness.baseUrl
        : `http://${options.remoteAuthority}:${new URL(harness.baseUrl).port}`
      await harness.page.goto(pageUrl, { waitUntil: 'domcontentloaded' })
      try {
        await harness.page.locator('[class*="frame"]').waitFor({ timeout: 15_000 })
      } catch {
        await harness.page.reload({ waitUntil: 'domcontentloaded' })
        await harness.page.locator('[class*="frame"]').waitFor({ timeout: 30_000 })
      }
      if (options.showWelcomeNotice !== true) {
        const declaration = harness.page.getByRole('dialog', { name: /Internal Testing Notice|内测声明/ })
        try {
          await declaration.waitFor({ timeout: 10_000 })
          await declaration.getByRole('button', { name: /Continue|继续/ }).click()
          await declaration.waitFor({ state: 'hidden', timeout: 15_000 })
        } catch (error) {
          if (await declaration.count() !== 0) throw error
        }
        if (options.preserveCredentialOnboarding !== true) {
          const credential = harness.page.getByRole('dialog', { name: /Add an API Key to get started|添加一个 API Key 开始使用/i })
          try {
            await credential.waitFor({ timeout: 3_000 })
            await credential.getByRole('button', { name: /Configure later|稍后配置/ }).click()
            await credential.waitFor({ state: 'hidden', timeout: 15_000 })
          } catch (error) {
            if (await credential.count() !== 0) throw error
          }
        }
      }
      return harness
    } catch (error) {
      await harness.close()
      throw error
    }
  }

  async rpc<T>(method: string, payload: Record<string, unknown> = {}): Promise<RpcResult<T>> {
    const response = await fetch(`${this.baseUrl}/api/${method}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        type: 'client-request',
        rpcId: `${method}-${crypto.randomUUID()}`,
        method,
        payload,
      }),
    })
    if (!response.ok) throw new Error(`${method} returned HTTP ${response.status}: ${await response.text()}`)
    const body = await response.json() as { result?: RpcResult<T> }
    if (body.result === undefined || typeof body.result.ok !== 'boolean') {
      throw new Error(`${method} returned an invalid response`)
    }
    return body.result
  }

  async sessions(): Promise<SessionListItem[]> {
    const result = await this.rpc<{ items: SessionListItem[] }>('session.list')
    if (!result.ok || result.value === undefined) throw new Error(`session.list failed: ${JSON.stringify(result.error)}`)
    return result.value.items
  }

  whenTurnSettled(timeout = 60_000): Promise<string> {
    return (async () => {
      const baseline = new Map((await this.sessions()).map(item => [item.sessionId, item.updatedAt]))
      const deadline = Date.now() + timeout
      while (Date.now() < deadline) {
        const candidates = (await this.sessions())
          .filter(item => !baseline.has(item.sessionId) || item.updatedAt > (baseline.get(item.sessionId) ?? 0))
          .sort((left, right) => right.updatedAt - left.updatedAt)
        const settled = candidates.find(item => !item.running)
        if (settled !== undefined) return settled.sessionId
        await Bun.sleep(50)
      }
      throw new Error('live turn did not settle')
    })()
  }
  async seedSession(id: string, recording: string): Promise<void> {
    await mkdir(this.dataDir, { recursive: true })
    const document = recording
      .replaceAll('{{sessionId}}', id)
      .replaceAll('{{cwd}}', this.workspace)
      .replaceAll('{{rpcId}}', 'seed')
      .replaceAll('{{system}}', '')
      .replaceAll('{{tools}}', '[]')
    const path = join(this.dataDir, `session-${Buffer.from(id).toString('hex')}.jsonl`)
    await writeFile(path, document.endsWith('\n') ? document : `${document}\n`)
    await chmod(path, 0o600)
  }


  assertClean(): void {
    expect(this.pageErrors).toEqual([])
    expect(this.warnings).toEqual([])
    expect(this.httpErrors).toEqual([])
  }


  async close(): Promise<void> {
    if (this.browser !== undefined) {
      await Promise.race([this.browser.close().catch(() => {}), Bun.sleep(5_000)])
    }
    if (this.server !== undefined) {
      this.server.kill('SIGINT')
      await Promise.race([this.server.exited, Bun.sleep(5_000)])
      if (this.server.exitCode === null) this.server.kill('SIGKILL')
      await this.server.exited
    }
    if (this.root !== undefined) await rm(this.root, { recursive: true, force: true })
  }
}

export function acknowledgeReloadConnectionLoss(harness: RustWebHarness, warningStart: number): void {
  const reloadWarnings = harness.warnings.splice(warningStart)
  harness.warnings.push(...reloadWarnings.filter(text => !/connection lost/i.test(text)))
}

export async function fixture(name: string, file = 'session.jsonl'): Promise<string> {
  const path = join(UPSTREAM_TESTS, 'snapshots', name, file)
  await readFile(path)
  return path
}

export function materializeRecording(recording: string): string {
  const rows = recording.trimEnd().split('\n').flatMap(line => {
    const row = JSON.parse(line) as Record<string, unknown>
    if (typeof row.type === 'string' && row.type.startsWith('compaction/')) row.ignorable = true
    if (typeof row.data === 'object' && row.data !== null) {
      const data = row.data as Record<string, unknown>
      if (typeof row.type === 'string' && row.type.startsWith('compaction/')) delete data.sourceCommandId
      if (row.type === 'user/message' && data.id === undefined) {
        data.id = `fixture-user-${row.seq}`
        data.role = 'user'
      }
      const source = typeof data.source === 'object' && data.source !== null ? data.source as Record<string, unknown> : undefined
      if (row.type === 'user/message' && source?.kind === 'user') {
        data.source = { kind: 'user', ...(typeof source.clientTimeZone === 'string' ? { clientTimeZone: source.clientTimeZone } : {}) }
      } else if (row.type === 'user/message' && source?.kind === 'plugin') {
        data.source = {
          kind: 'plugin', plugin: source.plugin, compactionId: source.compactionId,
          form: source.form, sections: source.sections, summary: source.summary,
        }
      } else if (row.type === 'user/message' && source?.kind === 'agent-instructions') {
        data.source = { kind: 'plugin', plugin: 'tessivum-workspace-instructions', form: 'instructions', summary: 'AGENTS.md' }
      }
      if (row.type === 'assistant/message' && data.message === undefined && Array.isArray(data.content)) {
        const provenance = typeof data.provenance === 'object' && data.provenance !== null
          ? data.provenance as Record<string, unknown>
          : {}
        const { content, provenance: _, ...rest } = data
        row.data = {
          ...rest,
          message: {
            id: `fixture-assistant-${row.seq}`,
            role: 'assistant',
            content,
            source: { kind: 'model', provider: provenance.provider, model: provenance.model },
          },
        }
      } else if (row.type === 'tool/result' && data.message === undefined && Array.isArray(data.content)) {
        const { callId, content, isError, ...rest } = data
        row.data = {
          ...rest,
          message: {
            id: `fixture-tool-result-${row.seq}`,
            role: 'user',
            content: [{ type: 'tool-result', toolCallId: callId, content, isError }],
            source: { kind: 'tool', callId },
          },
        }
      }
    }
    if (row.type !== 'reasoning-chunks' && row.type !== 'text-chunks' && row.type !== 'tool-call-chunks') return [row]
    if (typeof row.seq0 !== 'number' || typeof row.time0 !== 'number' || typeof row.data !== 'object' || row.data === null) {
      throw new Error(`compact recording row is malformed: ${line}`)
    }
    const data = row.data as Record<string, unknown>
    const values = row.type === 'tool-call-chunks' ? data.args : data.texts
    if (!Array.isArray(values) || !values.every(value => typeof value === 'string')) {
      throw new Error(`compact recording row has invalid deltas: ${line}`)
    }
    const dt = Array.isArray(data.dt) ? data.dt : []
    let time = row.time0
    return values.map((value, index) => {
      const chunk = row.type === 'tool-call-chunks'
        ? { type: 'tool-call-delta', index: data.index, ...(index === 0 ? { id: data.id, name: data.name } : {}), argumentsDelta: value }
        : { type: row.type === 'reasoning-chunks' ? 'reasoning-delta' : 'text-delta', index: data.index, text: value }
      const event = {
        type: 'assistant/chunk',
        seq: (row.seq0 as number) + index,
        time,
        data: { turn: data.turn, step: data.step, chunk },
      }
      const delay = dt[index]
      if (typeof delay === 'number') time += delay
      return event
    })
  })
  return `${rows.map(row => JSON.stringify(row)).join('\n')}\n`
}
export function settledRecording(title: string, user: string, assistant: string): string {
  const time = 1_785_000_000_000
  const rows = [
    { type: 'session', version: 0, id: '{{sessionId}}', createdAt: time, cwd: '{{cwd}}' },
    { type: 'turn/start', time, seq: 0, data: { turn: 1 } },
    { type: 'user/message', time: time + 1_000, seq: 1, data: { id: 'fixture-user', role: 'user', content: [{ type: 'text', text: user }], source: { kind: 'user' } }, surfaceOp: 'append' },
    { type: 'session/title', time: time + 2_000, seq: 2, data: { title, messageSeqs: [1], source: { kind: 'fallback' } } },
    { type: 'step/start', time: time + 3_000, seq: 3, data: { turn: 1, step: 1 } },
    { type: 'assistant/message', time: time + 4_000, seq: 4, data: { turn: 1, step: 1, message: { id: 'fixture-assistant', role: 'assistant', content: [{ type: 'text', text: assistant }], source: { kind: 'model', provider: 'fixture', model: 'fixture' } } }, surfaceOp: 'append' },
    { type: 'step/end', time: time + 5_000, seq: 5, data: { turn: 1, step: 1 } },
    { type: 'turn/end', time: time + 6_000, seq: 6, data: { turn: 1, reason: { kind: 'completed' } } },
  ]
  return rows.map(row => JSON.stringify(row)).join('\n') + '\n'
}

export interface SeededSubagent {
  childId: string
  label: string
  mode: 'continuable' | 'one-shot'
}

/** Durable catalog entries as emitted by the native SubagentService. */
export function withSubagents(parentId: string, recording: string, children: SeededSubagent[]): string {
  const rows = recording.trimEnd().split('\n').map(row => JSON.parse(row) as Record<string, unknown>)
  let seq = Math.max(...rows.map(row => Number(row.seq ?? -1))) + 1
  for (const child of children) {
    rows.push({
      type: 'subagent/contained-start',
      seq: seq++,
      time: 1_785_000_010_000 + seq,
      data: {
        child: {
          provider: 'native',
          agentId: child.label,
          parentSessionId: parentId,
          childSessionId: child.childId,
          mode: child.mode,
          capabilities: [],
          options: { provider: 'recorded', model: 'recorded' },
        },
      },
    })
  }
  return `${rows.map(row => JSON.stringify(row)).join('\n')}\n`
}

/** A persisted direct descendant that can be read without starting an agent. */
export function subagentRecording(parentId: string, title: string, user: string, assistant: string): string {
  const rows = settledRecording(title, user, assistant).trimEnd().split('\n').map(row => JSON.parse(row) as Record<string, unknown>)
  const header = rows[0]
  header.origin = 'subagent'
  header.parentSession = parentId
  header.delegationDepth = 1
  return `${rows.map(row => JSON.stringify(row)).join('\n')}\n`
}

/** One routed recorded completion; routes keep parent and child calls independent. */
export function textReplay(sessionId: string, text: string, requestId?: string): string {
  return JSON.stringify({
    sessionId,
    provider: 'recorded',
    model: 'recorded',
    requestId,
    chunks: [
      { type: 'block-start', index: 0, blockType: 'text' },
      { type: 'block-end', index: 0, block: { type: 'text', text } },
      { type: 'finish', reason: { kind: 'stop' } },
    ],
  })
}

export async function openSeededSession(harness: RustWebHarness, done: string): Promise<void> {
  const target = harness.page.getByText(done, { exact: true })
  if (await target.count() > 0) return
  const collapsed = harness.page.locator('[role="treeitem"][aria-expanded="false"]')
  while (await collapsed.count() > 0) await collapsed.first().click()
  const sessions = harness.page.locator('[role="treeitem"]:not([aria-expanded])')
  await waitUntil(() => sessions.count(), count => count > 0, 10_000)
  for (let index = await sessions.count() - 1; index >= 0; index -= 1) {
    await sessions.nth(index).click()
    try {
      await target.waitFor({ timeout: 5_000 })
      return
    } catch {}
  }
  throw new Error(`seeded session containing ${JSON.stringify(done)} was not found`)
}

export function stableAria(snapshot: string): string {
  return snapshot
    .replace(
      /~\d+(?:y(?: \d+mo)?|mo(?: \d+d)?)|\b(?:\d+d(?: \d+h(?: \d+m \d+s)?)?|\d+h \d+m \d+s|\d+m ?\d+s|\d+(?:\.\d+)?s|\d+(?:\.\d+)?ms)\b/g,
      duration => duration.startsWith('~') ? duration : '{{duration}}',
    )
    .replace(/\d+(?:\.\d+)?(?= tok\/s(?!\w))/g, '{{throughput}}')
    .replace(/\d{4}年\d{1,2}月\d{1,2}日 \d{2}:\d{2}/g, '{{clock}}')
    .replace(/\d{1,2}\/\d{1,2} \d{2}:\d{2}/g, '{{clock}}')
    .replace(/\d{1,2}月\d{1,2}日 \d{2}:\d{2}/g, '{{clock}}')
    .replace(/(?<!\d)\d{1,2}:\d{2}:\d{2}(?:\.\d+)?(?:\s*[AP]M)?(?!\d)/gi, '{{clock}}')
    .replace(/(?<!\d)\d{2}:\d{2}(?!\d)/g, '{{clock}}')
    .trim()
}

export async function captureStableAria(page: Page, selector: string): Promise<string> {
  const region = page.locator(selector).first()
  let previous = stableAria(await region.ariaSnapshot())
  await waitUntil(async () => {
    const current = stableAria(await region.ariaSnapshot())
    const settled = current === previous
    previous = current
    return settled
  }, settled => settled, 5_000)
  return previous
}


export async function waitUntil<T>(read: () => Promise<T>, accepts: (value: T) => boolean, timeout = 15_000): Promise<T> {
  const deadline = Date.now() + timeout
  while (Date.now() < deadline) {
    const value = await read()
    if (accepts(value)) return value
    await Bun.sleep(50)
  }
  throw new Error('condition did not become true')
}

export interface LongChatFixture {
  log: string
  title: string
  markers: {
    user(turn: number): string
    assistant(turn: number): string
    tool(turn: number, index: number): string
  }
  turns: number
}

export function longChatFixture(options: { markerPrefix: string; title: string; turns?: number }): LongChatFixture {
  const turns = options.turns ?? 88
  const suffix = (turn: number): string => String(turn).padStart(3, '0')
  const markers = {
    user: (turn: number): string => `CHAT_SCROLL_${options.markerPrefix}_USER_${suffix(turn)}`,
    assistant: (turn: number): string => `CHAT_SCROLL_${options.markerPrefix}_ASSISTANT_${suffix(turn)}`,
    tool: (turn: number, index: number): string => `CHAT_SCROLL_${options.markerPrefix}_TOOL_${suffix(turn)}_${index}`,
  }
  const time = 1_785_000_000_000
  let seq = 0
  const rows: unknown[] = [{ type: 'session', version: 0, id: '{{sessionId}}', createdAt: time, cwd: '{{cwd}}' }]
  const append = (type: string, data: unknown, surfaceOp?: string): void => {
    rows.push({ type, time: time + seq + 1, seq: seq++, data, ...(surfaceOp === undefined ? {} : { surfaceOp }) })
  }
  for (let turn = 1; turn <= turns; turn += 1) {
    append('turn/start', { turn })
    const user = markers.user(turn)
    append('user/message', {
      id: `user-${suffix(turn)}`, role: 'user', content: [{ type: 'text', text: `${user} Review the long-running conversation state for turn ${turn}.` }], source: { kind: 'user' },
    }, 'append')
    if (turn === 1) append('session/title', { title: options.title, messageSeqs: [seq - 1], source: { kind: 'fallback' } })
    append('step/start', { turn, step: 1 })
    append('request/header', { header: { config: { provider: 'fixture', model: 'fixture' }, system: `Synthetic long-chat request ${turn}.` }, reason: turn === 1 ? 'initial' : 'change' })
    if (turn % 8 === 0) {
      const calls = [1, 2].map(index => {
        const marker = markers.tool(turn, index)
        return { id: `chat-scroll-${suffix(turn)}-${index}`, marker, arguments: JSON.stringify({ command: `printf '${marker}\\n'`, description: marker }) }
      })
      append('assistant/message', {
        turn, step: 1, message: { id: `assistant-tools-${suffix(turn)}`, role: 'assistant', content: calls.map(call => ({ type: 'tool-call', id: call.id, name: 'bash', arguments: call.arguments })), source: { kind: 'model', provider: 'fixture', model: 'fixture' } },
      }, 'append')
      for (const call of calls) {
        const callSeq = seq
        append('tool/call', { turn, step: 1, callId: call.id, name: 'bash', arguments: call.arguments })
        append('tool/result', {
          turn, step: 1, message: { id: `tool-${call.id}`, role: 'user', content: [{ type: 'tool-result', toolCallId: call.id, content: [{ type: 'text', text: Array.from({ length: 12 }, (_, index) => `${call.marker} output line ${String(index + 1).padStart(2, '0')}`).join('\n') }], isError: false }], source: { kind: 'tool', callId: call.id } }, sourceEventSeqs: [callSeq],
        }, 'append')
      }
      append('step/end', { turn, step: 1 })
      append('step/start', { turn, step: 2 })
      append('assistant/message', {
        turn, step: 2, message: { id: `assistant-${suffix(turn)}`, role: 'assistant', content: [{ type: 'text', text: `${markers.assistant(turn)} Both tool results are accounted for. This settled response remains identifiable after paging.` }], source: { kind: 'model', provider: 'fixture', model: 'fixture' } }, usage: { inputTokens: 2_000 + turn, outputTokens: 200 },
      }, 'append')
      append('step/end', { turn, step: 2 })
    } else {
      const text = `${markers.assistant(turn)} The conversation remains readable after several paragraphs.\n\nTurn ${turn} deliberately carries enough prose to wrap at narrower viewport widths. The semantic marker stays near the start so geometry probes can find the same rendered row.\n\nThe closing paragraph makes this a realistic assistant response rather than a one-line list item.`
      for (const chunk of [
        { type: 'block-start', index: 0, blockType: 'text' },
        { type: 'text-delta', index: 0, text },
        { type: 'block-end', index: 0, block: { type: 'text', text } },
        { type: 'usage', usage: { inputTokens: 2_000 + turn, outputTokens: 200 } },
        { type: 'finish', reason: { kind: 'stop' } },
      ]) append('assistant/chunk', { turn, step: 1, chunk })
      append('assistant/message', {
        turn, step: 1, message: { id: `assistant-${suffix(turn)}`, role: 'assistant', content: [{ type: 'text', text }], source: { kind: 'model', provider: 'fixture', model: 'fixture' } }, usage: { inputTokens: 2_000 + turn, outputTokens: 200 },
      }, 'append')
      append('step/end', { turn, step: 1 })
    }
    append('turn/end', { turn, reason: { kind: 'completed' } })
  }
  return { log: rows.map(row => JSON.stringify(row)).join('\n'), markers, title: options.title, turns }
}

export async function openSessionByMarker(harness: RustWebHarness, marker: string, tailMarker?: string): Promise<void> {
  const searchButton = harness.page.getByRole('button', { name: 'Search sessions' })
  if (await searchButton.getAttribute('aria-expanded') !== 'true') await searchButton.click()
  await harness.page.getByRole('textbox', { name: 'Search sessions...', exact: true }).fill(marker)
  const result = harness.page.getByRole('tree', { name: 'Search results' }).getByRole('treeitem')
  await waitUntil(() => result.count(), count => count === 1, 60_000)
  await result.click()
  await harness.page.getByRole('tab', { name: 'Chat', exact: true }).waitFor({ timeout: 30_000 })
  if (tailMarker !== undefined) await harness.page.getByText(tailMarker, { exact: false }).last().waitFor({ timeout: 30_000 })
}
