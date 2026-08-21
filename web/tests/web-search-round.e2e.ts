import { readFile, readdir } from 'node:fs/promises'
import { createServer, type Server } from 'node:http'
import type { AddressInfo } from 'node:net'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

const SNAPSHOT_DIR = join(import.meta.dir, 'snapshots/web-search-round')
const FIXTURE = join(SNAPSHOT_DIR, 'session.jsonl')
const PROMPT = 'Use web_search to search exactly "DeepSeek Harness snapshot search". Then reply exactly SEARCH_DONE and stop.'
const QUERY = 'DeepSeek Harness snapshot search'
const MAX_RESULTS = 8
const PROVIDER_RESULT_COUNT = 12

type Event = { type: string; data: Record<string, any> }
type SearchRequest = { path: string; apiKey?: string; body: Record<string, unknown> }

function resultUrl(ordinal: number): string {
  return `https://docs.example.test/search/${ordinal}`
}

function resultTitle(ordinal: number): string {
  return `Snapshot Search Result ${ordinal}`
}

function resultSnippet(ordinal: number): string {
  return `Snapshot search excerpt ${ordinal}: the harness replays this source list from a local endpoint.`
}

function resultPageAge(ordinal: number): string {
  return `2026-07-${String(ordinal).padStart(2, '0')}`
}

async function fixturePrompts(): Promise<string[]> {
  return (await readFile(FIXTURE, 'utf8'))
    .trim()
    .split('\n')
    .map(line => JSON.parse(line) as { type?: string; data?: { content?: Array<{ type?: string; text?: string }> } })
    .filter(row => row.type === 'user/message')
    .flatMap(row => row.data?.content ?? [])
    .flatMap(block => block.type === 'text' && block.text !== undefined ? [block.text] : [])
}

async function startSearchServer(requests: SearchRequest[]): Promise<{ server: Server; baseUrl: string }> {
  const server = createServer((request, response) => {
    let body = ''
    request.setEncoding('utf8')
    request.on('data', (chunk: string) => { body += chunk })
    request.on('end', () => {
      requests.push({
        path: request.url ?? '',
        apiKey: typeof request.headers['x-api-key'] === 'string' ? request.headers['x-api-key'] : undefined,
        body: JSON.parse(body) as Record<string, unknown>,
      })
      response.writeHead(200, { 'content-type': 'application/json' })
      response.end(JSON.stringify({
        content: [
          {
            type: 'text',
            text: `Found ${PROVIDER_RESULT_COUNT} sources.`,
            citations: Array.from({ length: PROVIDER_RESULT_COUNT }, (_, index) => index + 1).map(ordinal => ({
              type: 'web_search_result_location', url: resultUrl(ordinal), cited_text: resultSnippet(ordinal),
            })),
          },
          {
            type: 'web_search_tool_result',
            content: Array.from({ length: PROVIDER_RESULT_COUNT }, (_, index) => index + 1).map(ordinal => ({
              type: 'web_search_result', url: resultUrl(ordinal), title: resultTitle(ordinal), page_age: resultPageAge(ordinal),
            })),
          },
        ],
      }))
    })
  })
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => {
      server.off('error', reject)
      resolve()
    })
  })
  const address = server.address() as AddressInfo
  return { server, baseUrl: `http://127.0.0.1:${address.port}` }
}

async function close(server: Server): Promise<void> {
  await new Promise<void>((resolve, reject) => server.close(error => error === undefined ? resolve() : reject(error)))
}

async function events(harness: RustWebHarness, sessionId: string): Promise<Event[]> {
  const history = await harness.rpc<{ events: Array<{ event: Event }> }>('session.history', { sessionId, maxMessages: 1_000 })
  if (!history.ok || history.value === undefined) throw new Error(`session.history failed: ${JSON.stringify(history.error)}`)
  return history.value.events.map(entry => entry.event)
}

test('web search uses the real DeepSeek seam, preserves citations, and projects capped sources', async () => {
  expect(await fixturePrompts()).toEqual([PROMPT])
  const requests: SearchRequest[] = []
  const search = await startSearchServer(requests)
  const credential = crypto.randomUUID()
  let harness: RustWebHarness | undefined
  try {
    harness = await RustWebHarness.launch({
      name: 'web-search-round',
      locale: 'en-US',
      replayFixture: FIXTURE,
      deepSeekSearch: { baseURL: search.baseUrl, apiKeyEnv: 'DSH_WEB_SEARCH_E2E_KEY', apiKey: credential },
      env: { TESSIVUM_REPLAY_PACE_MS: '15' },
    })
    const input = harness.page.locator('textarea').first()
    const settled = harness.whenTurnSettled(180_000)
    await input.fill(PROMPT)
    await input.press('Enter')
    const sessionId = await settled

    await harness.page.getByText('SEARCH_DONE', { exact: true }).waitFor({ timeout: 15_000 })
    await harness.page.locator('[data-tool="web_search"]').waitFor({ timeout: 15_000 })
    expect(requests).toHaveLength(1)
    expect(requests[0]).toMatchObject({
      path: '/messages',
      apiKey: credential,
      body: {
        messages: [{ role: 'user', content: [{ type: 'text', text: `Perform a web search for the query: ${QUERY}` }] }],
        tools: [{ type: 'web_search_20250305', name: 'web_search' }],
      },
    })

    const log = await events(harness, sessionId)
    const request = log.find(event => event.type === 'web/deepseek-search-llm-request')
    expect(request?.data).toEqual({ endpoint: `${search.baseUrl}/messages`, apiVersion: '2023-06-01', body: requests[0]?.body })
    const searchCall = log.find(event => event.type === 'tool/call' && event.data.name === 'web_search')
    if (searchCall === undefined) throw new Error('the replayed turn did not call web_search')
    const searchResult = log.find(event => event.type === 'tool/result' && event.data.message?.source?.callId === searchCall.data.callId)
    if (searchResult === undefined) throw new Error('web_search produced no durable result')
    const rendered = JSON.stringify(searchResult.data.message)
    for (let ordinal = 1; ordinal <= MAX_RESULTS; ordinal += 1) {
      expect(rendered).toContain(`[${resultTitle(ordinal)}](${resultUrl(ordinal)})`)
    }
    for (let ordinal = MAX_RESULTS + 1; ordinal <= PROVIDER_RESULT_COUNT; ordinal += 1) {
      expect(rendered).not.toContain(resultUrl(ordinal))
    }
    expect(rendered).toContain(`(Showing the first ${MAX_RESULTS} sources. Refine the query for more.)`)
    expect(searchResult.data.meta).toMatchObject({
      sources: Array.from({ length: MAX_RESULTS }, (_, index) => index + 1).map(ordinal => ({
        url: resultUrl(ordinal), title: resultTitle(ordinal), snippet: resultSnippet(ordinal), publishedAt: resultPageAge(ordinal),
      })),
      truncated: true,
    })

    const row = harness.page.locator('[data-tool="web_search"] [data-expandable]').first()
    await row.click()
    expect(await waitUntil(() => row.getAttribute('aria-expanded'), value => value === 'true')).toBe('true')
    const card = harness.page.locator('[data-web="search"]')
    const sources = card.locator('ol')
    await sources.waitFor({ timeout: 10_000 })
    expect(await sources.locator('li').count()).toBe(MAX_RESULTS)
    expect(await card.locator('button').count()).toBe(0)
    expect(await card.getByText('来源列表已截断').isVisible()).toBe(true)
    const geometry = await sources.evaluate(element => {
      const style = getComputedStyle(element)
      return { maxHeight: style.maxHeight, overflowY: style.overflowY, scrollHeight: element.scrollHeight, clientHeight: element.clientHeight }
    })
    expect(geometry.maxHeight).toBe('320px')
    expect(geometry.overflowY).toBe('auto')
    expect(geometry.scrollHeight).toBeGreaterThan(geometry.clientHeight)
    const marker = await sources.evaluate(element => {
      const probe = document.createElement('span')
      probe.style.cssText = 'position:absolute;visibility:hidden;white-space:pre;font:inherit'
      probe.textContent = '999. '
      element.append(probe)
      const widest = probe.getBoundingClientRect().width
      probe.remove()
      return { widest, paddingLeft: parseFloat(getComputedStyle(element).paddingLeft) }
    })
    expect(marker.paddingLeft).toBeGreaterThanOrEqual(marker.widest)
    expect((await readdir(SNAPSHOT_DIR)).sort()).toEqual(['session.jsonl', 'ui.expected.md'])
    harness.assertClean()
  } finally {
    await harness?.close()
    await close(search.server)
  }
}, 240_000)
