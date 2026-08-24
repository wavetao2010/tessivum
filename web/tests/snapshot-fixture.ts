import { openSessionByMarker, type RustWebHarness } from './support'

const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAKAAAABaCAYAAAA/xl1SAAAAvklEQVR42u3SMQ0AAAjAMIyhELM4AAe8PD1qYFlk9cCXEAEDYkAwIAYEA2JAMCAGBANiQDAgBgQDYkAwIAYEA2JAMCAGBANiQDAgBgQDYkAwIAYEA2JAMCAGBANiQDAgBgQDYkAwIAYEA2JAMCAGxIBCYEAMCAbEgGBADAgGxIBgQAwIBsSAYEAMCAbEgGBADAgGxIBgQAwIBsSAYEAMCAbEgGBADAgGxIBgQAwIBsSAYEAMCAbEgGBADAgGxIAYEAyIAcGAGBAMiAHBgBgQDIgBwYAYEAyIAcGAGBAMiAHBgBgQDIgB4bYWLb6pnOb1xAAAAABJRU5ErkJggg==', 'base64')

export async function seedSnapshotFixture(harness: RustWebHarness): Promise<void> {
  const response = await fetch(`${harness.baseUrl}/api/attachments`, {
    method: 'POST',
    headers: { 'content-type': 'image/png', 'x-attachment-name': 'fixture-image.png' },
    body: PNG,
  })
  if (!response.ok) throw new Error(`fixture attachment upload failed: ${response.status}`)
  const attachment = await response.json() as Record<string, unknown>
  const recording = snapshotRecording(attachment)
  await harness.seedSession('snapshot-fixture', recording)
  await harness.page.reload({ waitUntil: 'load' })
  const credential = harness.page.getByRole('dialog', { name: /Add an API Key to get started|添加一个 API Key 开始使用/i })
  try {
    await credential.waitFor({ timeout: 3_000 })
    await credential.getByRole('button', { name: /Configure later|稍后配置/ }).click()
    await credential.waitFor({ state: 'hidden', timeout: 15_000 })
  } catch (error) {
    if (await credential.count() !== 0) throw error
  }
}

export async function openSnapshotSession(harness: RustWebHarness): Promise<void> {
  await openSessionByMarker(harness, '问题 74：todo_write 样本。')
  const search = harness.page.getByRole('button', { name: 'Search sessions' })
  if (await search.getAttribute('aria-expanded') === 'true') await search.click()
}

function snapshotRecording(attachment: Record<string, unknown>): string {
  const time = 1_786_406_400_000
  const rows: Record<string, unknown>[] = [{ type: 'session', version: 0, id: '{{sessionId}}', createdAt: time, cwd: '{{cwd}}' }]
  const append = (type: string, data: unknown, surfaceOp?: string, sourceEventSeqs?: number[]): void => {
    rows.push({ type, seq: rows.length - 1, time: time + rows.length, data, ...(surfaceOp === undefined ? {} : { surfaceOp }), ...(sourceEventSeqs === undefined ? {} : { sourceEventSeqs }) })
  }
  const message = (role: 'user' | 'assistant', content: unknown[], id: string): Record<string, unknown> => ({
    id, role, content, source: role === 'user' ? { kind: 'user' } : { kind: 'model', provider: 'fixture', model: 'fixture' },
  })
  const text = (value: string): Record<string, string> => ({ type: 'text', text: value })
  const toolTurn = (turn: number, name: string, args: Record<string, unknown>, output: string, meta?: Record<string, unknown>): void => {
    const callId = `fixture-call-${turn}`
    append('turn/start', { turn })
    append('user/message', { ...message('user', [text(`问题 ${turn}：${name} 样本。`)], `fixture-user-${turn}`) }, 'append')
    append('step/start', { turn, step: 0 })
    append('assistant/message', { turn, step: 0, message: message('assistant', [{ type: 'tool-call', id: callId, name, arguments: JSON.stringify(args) }], `fixture-assistant-${turn}`) }, 'append')
    const callSeq = rows.length - 1
    append('tool/call', { turn, step: 0, callId, name, arguments: JSON.stringify(args) })
    append('tool/result', { turn, step: 0, message: { role: 'user', content: [{ type: 'tool-result', toolCallId: callId, content: [{ type: 'text', text: output }], isError: false }], source: { kind: 'tool', callId }, id: `fixture-result-${turn}` }, ...(meta === undefined ? {} : { meta }) }, 'append', [callSeq])
    append('step/end', { turn, step: 0 })
    append('turn/end', { turn, reason: { kind: 'completed' } })
  }

  append('request/context', { provider: 'fixture', model: 'fixture', contextWindow: 128_000 })
  append('turn/start', { turn: 1 })
  append('user/message', { ...message('user', [text('Give one useful answer.')], 'fixture-title-user') }, 'append')
  append('session/title', { title: 'Fixture 历史会话', messageSeqs: [2], source: { kind: 'fallback' } })
  append('step/start', { turn: 1, step: 0 })
  append('assistant/message', { turn: 1, step: 0, message: message('assistant', [text('Fixture history ready.')], 'fixture-title-assistant') }, 'append')
  append('step/end', { turn: 1, step: 0 })
  append('turn/end', { turn: 1, reason: { kind: 'completed' } })

  toolTurn(2, 'bash', { command: 'pnpm run check', cwd: '/tmp/fixture/deep/nested' }, 'Running checks\n1 of 4 checks failed')
  const searchFiles = [
    { path: 'packages/client/ui-primitives/src/SearchBlock.tsx', matches: [
      { lineNumber: 16, line: 'export const DEFAULT_SEARCH_MAX_LINES = 16' },
      { lineNumber: 138, line: 'export function SearchBlock(props: SearchBlockProps) {' },
      { lineNumber: 141, line: '  const [collapsed, setCollapsed] = useState<ReadonlySet<number>>(() => new Set())' },
    ] },
    { path: 'packages/client/ui-tool/src/client/tool/models/search-card-model.ts', matches: [
      { lineNumber: 45, line: 'export const CHAT_SEARCH_MAX_LINES = 8' },
      { lineNumber: 130, line: 'export function searchCardModel(block: ToolCallBlock): SearchCardModel | null {' },
    ] },
    { path: 'packages/client/ui-tool/src/client/tool/toolviews/search-row.tsx', matches: [
      { lineNumber: 34, line: 'export function SearchRow({ toolName, block, inspect, t }: SearchRowProps) {' },
      { lineNumber: 36, line: '  const search = searchCardModel(block)' },
      { lineNumber: 56, line: '      search={search}' },
      { lineNumber: 78, line: "      yield ctx.slots.register({ name: 'tool.call.toolview', key: 'grep', locale: NS }, SearchRow)" },
    ] },
  ]
  const searchOutput = [
    'Found 9 of 42 matches',
    '',
    ...searchFiles.flatMap(file => [file.path, ...file.matches.map(match => `Line ${match.lineNumber}: ${match.line}`), '']),
    '(Full grep result stored at: fixture://spill/grep-66. Read it to see every match.)',
  ].join('\n')
  toolTurn(3, 'grep', { pattern: 'SEARCH_MAX_LINES', path: 'packages/client' }, searchOutput, {
    shape: 'matches', files: searchFiles, truncated: true, total: 42,
  })
  append('turn/start', { turn: 4 })
  append('user/message', { ...message('user', [text('问题 72：请完整列出全部一百条条目。')], 'fixture-user-72') }, 'append')
  append('step/start', { turn: 4, step: 0 })
  append('assistant/message', { turn: 4, step: 0, message: message('assistant', [text('条目 1：第一条。条目 2：第二条。条目 3：这一条写到一半被')], 'fixture-assistant-72') }, 'append')
  append('step/end', { turn: 4, step: 0 })
  append('turn/end', { turn: 4, reason: { kind: 'max-tokens' } })

  const imageRef = attachment
  append('turn/start', { turn: 5 })
  append('user/message', { ...message('user', [{ type: 'image', attachment: imageRef }, text('历史用户图片')], 'fixture-user-73') }, 'append')
  append('step/start', { turn: 5, step: 0 })
  append('assistant/message', { turn: 5, step: 0, message: message('assistant', [text('结构化模型图片：'), { type: 'image', attachment: imageRef }], 'fixture-assistant-73') }, 'append')
  append('step/end', { turn: 5, step: 0 })
  append('turn/end', { turn: 5, reason: { kind: 'completed' } })

  const todos = [
    { content: '梳理需求', status: 'completed' },
    { content: '实现 fixture 样本', status: 'in_progress' },
    { content: '跑后台构建', status: 'in_progress' },
    { content: '浏览器验收', status: 'pending' },
  ]
  const todoArgs = { todos }
  append('turn/start', { turn: 6 })
  append('user/message', { ...message('user', [text('问题 74：todo_write 样本。')], 'fixture-user-74') }, 'append')
  append('step/start', { turn: 6, step: 0 })
  append('assistant/message', { turn: 6, step: 0, message: message('assistant', [{ type: 'tool-call', id: 'fixture-call-74', name: 'todo_write', arguments: JSON.stringify(todoArgs) }], 'fixture-assistant-74') }, 'append')
  const todoCallSeq = rows.length - 1
  append('tool/call', { turn: 6, step: 0, callId: 'fixture-call-74', name: 'todo_write', arguments: JSON.stringify(todoArgs) })
  append('todo/write', { todos })
  append('tool/result', { turn: 6, step: 0, message: { role: 'user', content: [{ type: 'tool-result', toolCallId: 'fixture-call-74', content: [{ type: 'text', text: 'Updated todo list: 1 pending, 2 in progress, 1 completed.' }], isError: false }], source: { kind: 'tool', callId: 'fixture-call-74' }, id: 'fixture-result-74' } }, 'append', [todoCallSeq])
  append('step/end', { turn: 6, step: 0 })
  append('turn/end', { turn: 6, reason: { kind: 'completed' } })
  rows.forEach((row, index) => { row.seq = index - 1 })
  return rows.map(row => JSON.stringify(row)).join('\n')
}

