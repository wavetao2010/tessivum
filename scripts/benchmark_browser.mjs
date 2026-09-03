#!/usr/bin/env bun
import { createRequire } from 'node:module'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const now = () => Math.round(performance.now())

function parseArgs(argv) {
  const options = { timeoutMs: 60_000, sessions: 1, settleMs: 250 }
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index]
    if (!['--url', '--prompt', '--marker', '--timeout-ms', '--sessions', '--settle-ms', '--checkpoint'].includes(flag)) {
      throw new Error(`unknown option: ${flag}`)
    }
    const value = argv[++index]
    if (value === undefined || value.startsWith('--')) throw new Error(`${flag} requires a value`)
    if (flag === '--url') options.url = value
    else if (flag === '--prompt') options.prompt = value
    else if (flag === '--marker') options.marker = value
    else if (flag === '--checkpoint') options.checkpoint = value
    else {
      const parsed = Number(value)
      if (!Number.isSafeInteger(parsed) || parsed <= 0) throw new Error(`${flag} must be a positive integer`)
      if (flag === '--timeout-ms') options.timeoutMs = parsed
      else if (flag === '--sessions') options.sessions = parsed
      else options.settleMs = parsed
    }
  }
  if (typeof options.url !== 'string') throw new Error('--url is required')
  if (options.prompt !== undefined && typeof options.marker !== 'string') {
    throw new Error('--marker is required when --prompt is set')
  }
  if (options.sessions !== 1 && typeof options.prompt !== 'string') throw new Error('--sessions requires --prompt')
  if (typeof options.checkpoint !== 'string') throw new Error('--checkpoint is required')
  return options
}

async function dismiss(page, name, button, timeoutMs) {
  const dialog = page.getByRole('dialog', { name })
  try {
    await dialog.waitFor({ timeout: timeoutMs })
  } catch (error) {
    if (await dialog.count() !== 0) throw error
    return false
  }
  await dialog.getByRole('button', { name: button }).click()
  await dialog.waitFor({ state: 'hidden', timeout: timeoutMs })
  return true
}

async function main() {
  const result = {
    schema: 'tessivum.product-benchmark-browser/v1',
    timestamps: { startedMs: now() },
    errors: [],
  }
  let browser
  try {
    const options = parseArgs(process.argv.slice(2))
    result.url = options.url
    result.marker = options.marker ?? null
    result.promptSubmitted = false
    result.sessionsRequested = options.sessions
    result.sessionsCompleted = 0

    const here = dirname(fileURLToPath(import.meta.url))
    const require = createRequire(resolve(here, '../web/package.json'))
    const { chromium } = require('playwright-core')
    const executablePath = process.env.TESSIVUM_CHROMIUM
    browser = await chromium.launch(executablePath === undefined
      ? { channel: 'chrome' }
      : { executablePath })
    result.timestamps.browserLaunchedMs = now()

    const page = await browser.newPage({ locale: 'en-US', viewport: { width: 1680, height: 1000 } })
    page.on('dialog', dialog => void dialog.dismiss())
    page.on('pageerror', error => result.errors.push(`pageerror: ${error.message}`))
    await page.goto(options.url, { waitUntil: 'domcontentloaded', timeout: options.timeoutMs })
    const frame = page.locator('[class*="frame"]')
    try {
      await frame.waitFor({ timeout: Math.min(options.timeoutMs, 15_000) })
    } catch {
      await page.reload({ waitUntil: 'domcontentloaded', timeout: options.timeoutMs })
      await frame.waitFor({ timeout: options.timeoutMs })
    }
    result.timestamps.pageLoadedMs = now()
    await dismiss(page, /Internal Testing Notice|内测声明/, /Continue|继续/, Math.min(options.timeoutMs, 10_000))
    await dismiss(page, /Add an API Key to get started|添加一个 API Key 开始使用/i, /Configure later|稍后配置/, Math.min(options.timeoutMs, 10_000))

    const composer = page.locator('textarea:enabled').last()
    await composer.waitFor({ timeout: options.timeoutMs })
    result.timestamps.composerEnabledMs = now()
    if (options.prompt !== undefined) {
      await composer.fill(options.prompt)
      await composer.press('Enter')
      result.promptSubmitted = true
      result.timestamps.promptSubmittedMs = now()
      await page.getByText(options.marker, { exact: true }).last().waitFor({ timeout: options.timeoutMs })
      result.sessionsCompleted = 1
      result.timestamps.markerSeenMs = now()
      result.timestamps.lastMarkerSeenMs = result.timestamps.markerSeenMs
      await new Promise(resolve => setTimeout(resolve, options.settleMs))
      await Bun.write(`${options.checkpoint}.1`, `${JSON.stringify({ sessions: 1 })}\n`)
      for (let count = 2; count <= options.sessions; count += 1) {
        const completed = await page.evaluate(async ({ current, marker, prompt, timeoutMs }) => {
          let sequence = 0
          const rpc = async (method, payload) => {
            const response = await fetch(`/api/${method}`, {
              method: 'POST',
              headers: { 'content-type': 'application/json' },
              body: JSON.stringify({ type: 'client-request', rpcId: `benchmark-${current}-${++sequence}`, method, payload }),
            })
            return response.json()
          }
          const created = await rpc('session.create', {})
          if (created?.result?.ok !== true || typeof created.result.value?.sessionId !== 'string') return { error: created }
          const sessionId = created.result.value.sessionId
          const submitted = await rpc('session.prompt', { sessionId, mode: 'queue', content: [{ type: 'text', text: prompt }] })
          if (submitted?.result?.ok !== true) return { error: submitted }
          const deadline = Date.now() + timeoutMs
          while (Date.now() < deadline) {
            const history = await rpc('session.history', { sessionId })
            if (history?.result?.ok !== true) return { error: history }
            if (JSON.stringify(history).includes(marker)) return { sessionId }
            await new Promise(resolve => setTimeout(resolve, 25))
          }
          return { error: `session ${sessionId} did not complete before timeout` }
        }, { current: count, marker: options.marker, prompt: options.prompt, timeoutMs: options.timeoutMs })
        if (typeof completed?.sessionId !== 'string') throw new Error(`resident session ${count} failed: ${JSON.stringify(completed?.error)}`)
        result.sessionsCompleted = count
      }
      await new Promise(resolve => setTimeout(resolve, options.settleMs))
      await Bun.write(`${options.checkpoint}.${result.sessionsCompleted}`, `${JSON.stringify({ sessions: result.sessionsCompleted })}\n`)
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    result.errors.push(message)
    process.stderr.write(`benchmark browser: ${message}\n`)
    process.exitCode = 1
  } finally {
    if (browser !== undefined) {
      try {
        await browser.close()
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error)
        result.errors.push(`browser close: ${message}`)
        process.stderr.write(`benchmark browser: ${message}\n`)
        process.exitCode = 1
      }
    }
    result.timestamps.finishedMs = now()
    process.stdout.write(`${JSON.stringify(result)}\n`)
  }
}

await main()

