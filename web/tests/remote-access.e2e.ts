import { afterAll, expect, test } from 'bun:test'
import { chmod, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import type { BrowserContext, Page } from 'playwright-core'
import { RustWebHarness } from './support.ts'

let harness: RustWebHarness | undefined
let mobileContext: BrowserContext | undefined

interface RemoteRpcBody {
  ok: boolean
  output?: {
    token?: string
    device?: { name: string, createdAt: number, expiresAt: number }
    devices?: unknown[]
    enabled?: boolean
    authority?: string
  }
  error?: { code: string }
}

async function remoteRpc(
  baseUrl: string,
  method: string,
  args: object,
  headers: Record<string, string> = {},
): Promise<{ status: number, cookie: string | null, body: RemoteRpcBody }> {
  const response = await fetch(`${baseUrl}/api/remoteAccess/${method}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...headers },
    body: JSON.stringify({ requestId: crypto.randomUUID(), args }),
  })
  return {
    status: response.status,
    cookie: response.headers.get('set-cookie'),
    body: await response.json() as RemoteRpcBody,
  }
}

async function waitForDevice(page: Page, name: string): Promise<void> {
  await page.getByText(name, { exact: true }).waitFor({ timeout: 10_000 })
}

test('Rust-owned Remote Access UI enables, remembers, pairs, and revokes', async () => {
  const tunnelRoot = await mkdtemp(join(tmpdir(), 'tessivum-fake-cloudflared-'))
  const fakeCloudflared = join(tunnelRoot, 'cloudflared')
  await writeFile(fakeCloudflared, '#!/bin/sh\nif [ -f "$0.fail" ]; then echo "tunnel unavailable" >&2; exit 1; fi\necho "https://remote-test.trycloudflare.com" >&2\ntrap "exit 0" TERM INT\nwhile :; do sleep 1; done\n')
  await chmod(fakeCloudflared, 0o700)
  try {
    const disabledHarness = await RustWebHarness.launch({
      name: 'remote-access-disabled',
      env: { TESSIVUM_CLOUDFLARED: fakeCloudflared },
    })
    try {
      const disabled = await remoteRpc(disabledHarness.baseUrl, 'describe', {})
      expect(disabled.status).toBe(200)
      expect(disabled.body.output?.enabled).toBe(false)
      await disabledHarness.page.goto(`${disabledHarness.baseUrl}/remote`)
      await disabledHarness.page.getByRole('heading', { name: 'Remote Access is off' }).waitFor()
      const logo = disabledHarness.page.getByRole('link', { name: 'Tessivum home' }).locator('img')
      expect(await logo.getAttribute('src')).toBe('/favicon.svg')
      expect(await logo.evaluate(image => [image.naturalWidth, image.naturalHeight])).toEqual([32, 32])

      await disabledHarness.page.getByRole('button', { name: 'Enable with Cloudflare' }).click()
      await disabledHarness.page.getByRole('heading', { name: 'Create a pairing link' }).waitFor({ timeout: 60_000 })
      expect((await remoteRpc(disabledHarness.baseUrl, 'describe', {})).body.output?.enabled).toBe(true)

      await disabledHarness.page.getByRole('button', { name: 'Disable Remote Access' }).click()
      await disabledHarness.page.getByRole('heading', { name: 'Remote Access is off' }).waitFor({ timeout: 60_000 })
      expect((await remoteRpc(disabledHarness.baseUrl, 'describe', {})).body.output?.enabled).toBe(false)

      await writeFile(`${fakeCloudflared}.fail`, '')
      await disabledHarness.page.getByRole('button', { name: 'Enable with Cloudflare' }).click()
      await disabledHarness.page.getByText('REMOTE_TUNNEL_UNAVAILABLE', { exact: true }).waitFor({ timeout: 60_000 })
      expect((await remoteRpc(disabledHarness.baseUrl, 'describe', {})).body.output?.enabled).toBe(false)
      const unexpectedWarnings = disabledHarness.warnings.filter(warning => !warning.includes('net::ERR_CONNECTION_REFUSED'))
      disabledHarness.warnings.splice(0, disabledHarness.warnings.length, ...unexpectedWarnings)
      disabledHarness.assertClean()
    } finally {
      await disabledHarness.close()
    }
  } finally {
    await rm(tunnelRoot, { recursive: true, force: true })
  }

  harness = await RustWebHarness.launch({
    name: 'remote-access',
    env: {
      TESSIVUM_REMOTE_ACCESS: '1',
      TESSIVUM_REMOTE_TRUSTED_TUNNEL: '1',
      TESSIVUM_WEB_TRUSTED_AUTHORITIES: 'app.example.test',
      TESSIVUM_REMOTE_SESSION_TTL_SECONDS: '300',
    },
  })

  await harness.page.getByRole('button', { name: 'Settings', exact: true }).click()
  await harness.page.getByRole('button', { name: 'Remote access', exact: true }).click()
  await harness.page.waitForURL(`${harness.baseUrl}/remote`)
  await harness.page.getByRole('heading', { name: 'Create a pairing link' }).waitFor()

  await harness.page.getByRole('button', { name: 'Generate QR and link' }).click()
  const link = harness.page.getByRole('textbox', { name: 'Secure pairing link' })
  expect(await link.inputValue()).toMatch(/^https:\/\/app\.example\.test\/remote#pair=tvp_[a-f0-9]+$/)
  expect(await harness.page.locator('#qr svg').count()).toBe(1)
  const pairingUrl = await link.inputValue()

  mobileContext = await harness.browser.newContext({ viewport: { width: 320, height: 760 } })
  const mobileErrors: string[] = []
  const mobile = await mobileContext.newPage()
  mobile.on('pageerror', error => mobileErrors.push(error.message))
  await mobile.goto(`${harness.baseUrl}/remote?mobile=1${new URL(pairingUrl).hash}`)
  await mobile.getByRole('heading', { name: 'Pair this browser' }).waitFor()
  expect(await mobile.evaluate(() => location.hash)).toBe('')
  await mobile.getByRole('textbox', { name: 'Device name' }).fill('Mobile E2E')
  await mobile.getByRole('button', { name: 'Pair and open Tessivum' }).click()
  await mobile.waitForURL(`${harness.baseUrl}/`)
  expect(mobileErrors).toEqual([])
  const mobileSession = (await mobileContext.cookies()).find(cookie => cookie.name === '__Host-tessivum-remote')
  expect(mobileSession?.httpOnly).toBe(true)
  expect(mobileSession?.secure).toBe(true)
  expect(mobileSession?.sameSite).toBe('Strict')
  await mobile.goto(`${harness.baseUrl}/remote?reused=1${new URL(pairingUrl).hash}`)
  await mobile.getByRole('heading', { name: 'Pair this browser' }).waitFor()
  await mobile.getByRole('textbox', { name: 'Device name' }).fill('Reused link')
  await mobile.getByRole('button', { name: 'Pair and open Tessivum' }).click()
  await mobile.getByRole('alert').waitFor()
  expect(await mobile.getByRole('alert').textContent()).toContain('REMOTE_AUTH_REQUIRED')

  await harness.page.reload()
  await harness.page.getByRole('heading', { name: 'Paired devices' }).waitFor()
  await waitForDevice(harness.page, 'Mobile E2E')
  expect(await harness.page.getByText(/1 saved device · 1 active\./).count()).toBe(1)
  await harness.page.getByRole('button', { name: 'Revoke', exact: true }).click()
  await harness.page.getByText(/Revoked · Last active/).waitFor()
  expect(await harness.page.getByText(/1 saved device · 0 active\./).count()).toBe(1)
  expect(await harness.page.getByRole('button', { name: 'Revoke', exact: true }).count()).toBe(0)

  const issued = await remoteRpc(harness.baseUrl, 'issuePairing', {})
  expect(issued.status).toBe(200)
  const publicHeaders = {
    Host: 'app.example.test',
    Origin: 'https://app.example.test',
    'X-Forwarded-Proto': 'https',
  }
  const revokedBrowser = await remoteRpc(harness.baseUrl, 'describe', {}, {
    ...publicHeaders,
    Cookie: `${mobileSession?.name}=${mobileSession?.value}`,
  })
  expect([revokedBrowser.status, revokedBrowser.body.error?.code]).toEqual([401, 'REMOTE_SESSION_REVOKED'])
  const unauthenticated = await remoteRpc(harness.baseUrl, 'describe', {}, publicHeaders)
  expect(unauthenticated.status).toBe(200)
  expect(unauthenticated.body.output).toEqual({ enabled: true, authority: 'public' })
  const publicAsset = await fetch(`${harness.baseUrl}/favicon.svg`, { headers: publicHeaders })
  expect(publicAsset.status).toBe(200)
  const anonymousNodeRoute = await fetch(`${harness.baseUrl}/dsh-market/api/v1/capabilities`, { headers: publicHeaders })
  expect(anonymousNodeRoute.status).toBe(401)
  const anonymousNodeBody = await anonymousNodeRoute.json() as RemoteRpcBody
  expect(anonymousNodeBody.error?.code).toBe('REMOTE_AUTH_REQUIRED')
  const wrongHost = await remoteRpc(harness.baseUrl, 'describe', {}, {
    ...publicHeaders,
    Host: 'evil.example.test',
  })
  expect([wrongHost.status, wrongHost.body.error?.code]).toEqual([403, 'REMOTE_HOST_DENIED'])
  const wrongOrigin = await remoteRpc(harness.baseUrl, 'describe', {}, {
    ...publicHeaders,
    Origin: 'https://evil.example.test',
  })
  expect([wrongOrigin.status, wrongOrigin.body.error?.code]).toEqual([403, 'REMOTE_ORIGIN_DENIED'])
  const noTls = await remoteRpc(harness.baseUrl, 'describe', {}, {
    Host: publicHeaders.Host,
    Origin: publicHeaders.Origin,
  })
  expect([noTls.status, noTls.body.error?.code]).toEqual([403, 'REMOTE_TLS_REQUIRED'])
  const exchanged = await remoteRpc(harness.baseUrl, 'exchangePairing', {
    token: issued.body.output?.token,
    deviceName: 'Self revoke E2E',
  }, publicHeaders)
  expect(exchanged.status).toBe(200)
  expect((exchanged.body.output?.device?.expiresAt ?? 0) - (exchanged.body.output?.device?.createdAt ?? 0)).toBe(300_000)
  const cookie = exchanged.cookie?.split(';', 1)[0]
  expect(cookie?.startsWith('__Host-tessivum-remote=tvs_')).toBe(true)
  const authenticatedHeaders = { ...publicHeaders, Cookie: cookie ?? '' }
  const described = await remoteRpc(harness.baseUrl, 'describe', {}, authenticatedHeaders)
  expect(described.status).toBe(200)
  expect(described.body.output?.device?.name).toBe('Self revoke E2E')
  expect(described.body.output?.devices).toBeUndefined()
  const authenticatedNodeRoute = await fetch(`${harness.baseUrl}/dsh-market/api/v1/capabilities`, { headers: authenticatedHeaders })
  expect(authenticatedNodeRoute.status).toBe(403)
  const authenticatedNodeBody = await authenticatedNodeRoute.json() as RemoteRpcBody
  expect(authenticatedNodeBody.error?.code).toBe('REMOTE_HOST_DENIED')
  const remoteIssue = await remoteRpc(harness.baseUrl, 'issuePairing', {}, authenticatedHeaders)
  expect([remoteIssue.status, remoteIssue.body.error?.code]).toEqual([403, 'REMOTE_HOST_DENIED'])
  const settingsResponse = await fetch(`${harness.baseUrl}/api/settings.update`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...authenticatedHeaders },
    body: JSON.stringify({ requestId: crypto.randomUUID(), args: {} }),
  })
  const settingsBody = await settingsResponse.json() as RemoteRpcBody
  expect([settingsResponse.status, settingsBody.error?.code]).toEqual([403, 'REMOTE_HOST_DENIED'])
  const shutdownResponse = await fetch(`${harness.baseUrl}/api/host/shutdown`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...authenticatedHeaders },
    body: JSON.stringify({ requestId: crypto.randomUUID(), args: {} }),
  })
  const shutdownBody = await shutdownResponse.json() as RemoteRpcBody
  expect([shutdownResponse.status, shutdownBody.error?.code]).toEqual([403, 'REMOTE_HOST_DENIED'])
  const selfRevoked = await remoteRpc(harness.baseUrl, 'revokeSelf', {}, authenticatedHeaders)
  expect(selfRevoked.status).toBe(200)
  expect(selfRevoked.cookie).toContain('Max-Age=0')
  const afterRevoke = await remoteRpc(harness.baseUrl, 'describe', {}, authenticatedHeaders)
  expect(afterRevoke.status).toBe(401)
  expect(afterRevoke.body.error?.code).toBe('REMOTE_SESSION_REVOKED')

  expect(harness.pageErrors).toEqual([])
  expect(harness.warnings).toEqual([])
  expect(harness.httpErrors).toEqual([])
}, 180_000)

afterAll(async () => {
  await mobileContext?.close()
  await harness?.close()
})
