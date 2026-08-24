import { expect, test } from 'bun:test'
import { RustWebHarness } from './support'

const PROTOCOL_EXPECTED = `${import.meta.dir}/snapshots/message-feedback-protocol/protocol.expected.json`
const SESSION_FIXTURE = `${import.meta.dir}/snapshots/message-feedback-protocol/session.jsonl`
const SESSION_ID = 'message-feedback-protocol'
const MESSAGE_ID = '11111111-1111-4111-8111-111111111111'

type Exchange = { endpoint: string; request: unknown; status: number; response: unknown }

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null
}

function createdVersion(response: unknown): string {
  if (!isRecord(response) || !isRecord(response.result) || response.result.ok !== true
    || !isRecord(response.result.value) || response.result.value.ok !== true
    || !isRecord(response.result.value.value) || typeof response.result.value.value.version !== 'string') {
    throw new Error('messageFeedback.put did not return a successful versioned item')
  }
  return response.result.value.value.version
}

function normalize(exchanges: readonly Exchange[], version: string): string {
  return JSON.stringify(exchanges, (key, value: unknown) => {
    if ((key === 'version' || key === 'ifVersion') && value === version) return '{{version}}'
    if ((key === 'createdAt' || key === 'updatedAt') && typeof value === 'number') return '{{timestamp}}'
    return value
  }, 2)
}

test('snapshots strict list, put, conflict, and delete calls through the native Host Remote', async () => {
  const harness = await RustWebHarness.launch({ name: 'message-feedback-protocol-snapshot' })
  try {
    await harness.seedSession(SESSION_ID, await Bun.file(SESSION_FIXTURE).text())
    await harness.page.reload({ waitUntil: 'load' })
    const exchanges: Exchange[] = []
    const invoke = async (rpcId: string, endpoint: string, request: unknown): Promise<unknown> => {
      const payload = { args: { request } }
      const response = await fetch(`${harness.baseUrl}/api/${endpoint}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ type: 'client-request', rpcId, method: endpoint, payload }),
      })
      const body = await response.json() as unknown
      exchanges.push({ endpoint: `/api/${endpoint}`, request: payload, status: response.status, response: body })
      return body
    }
    await invoke('feedback-invalid', 'messageFeedback/put', { sessionId: SESSION_ID, messageId: MESSAGE_ID, rating: 'invalid-rating', ifVersion: null })
    await invoke('feedback-list-empty', 'messageFeedback/list', { sessionId: SESSION_ID })
    const created = await invoke('feedback-put', 'messageFeedback/put', { sessionId: SESSION_ID, messageId: MESSAGE_ID, rating: 'positive', note: 'Useful answer', ifVersion: null })
    const version = createdVersion(created)
    expect(version).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/)
    await invoke('feedback-list-created', 'messageFeedback/list', { sessionId: SESSION_ID })
    await invoke('feedback-conflict', 'messageFeedback/put', { sessionId: SESSION_ID, messageId: MESSAGE_ID, rating: 'negative', ifVersion: null })
    await invoke('feedback-delete', 'messageFeedback/delete', { sessionId: SESSION_ID, messageId: MESSAGE_ID, ifVersion: version })
    await invoke('feedback-list-deleted', 'messageFeedback/list', { sessionId: SESSION_ID })
    expect(exchanges.every(exchange => exchange.status === 200)).toBe(true)
    const actual = normalize(exchanges, version)
    if (process.env.TESSIVUM_UPDATE_GOLDENS === '1') await Bun.write(PROTOCOL_EXPECTED, `${actual}\n`)
    expect(actual).toBe((await Bun.file(PROTOCOL_EXPECTED).text()).trim())
    harness.assertClean()
  } finally {
    await harness.close()
  }
})
