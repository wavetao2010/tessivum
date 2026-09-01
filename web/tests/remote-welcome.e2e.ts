import { expect, test } from 'bun:test'
import { acknowledgeReloadConnectionLoss, RustWebHarness, waitUntil } from './support'


test('remote welcome advances process-locally and returns after reload', async () => {
  const harness = await RustWebHarness.launch({
    name: 'remote-welcome',
    locale: 'zh-CN',
    remoteAuthority: 'remote.localhost',
    viewport: { width: 320, height: 760 },
    showWelcomeNotice: true,
  })
  try {
    const welcome = harness.page.getByRole('dialog', { name: '内测声明' })
    await welcome.waitFor({ timeout: 15_000 })
    expect(await harness.page.locator('#root').evaluate(root => (root as HTMLElement).inert)).toBe(true)

    await welcome.getByRole('button', { name: '继续' }).click()
    await welcome.waitFor({ state: 'detached', timeout: 15_000 })
    await waitUntil(
      () => harness.page.locator('#root').evaluate(root => (root as HTMLElement).inert),
      inert => !inert,
    )

    const openSidebar = harness.page.getByRole('button', { name: /打开侧边栏|Open sidebar/ })
    if (await openSidebar.count() !== 0) await openSidebar.click()
    expect(await harness.page.getByRole('treeitem', { name: 'workspace', exact: true }).count()).toBe(1)
    expect(await harness.page.getByRole('button', { name: /新.*会话/ }).count()).toBeGreaterThan(0)
    await harness.page.getByRole('button', { name: '设置', exact: true }).last().click()
    const settings = harness.page.getByRole('dialog', { name: '设置' })
    await settings.getByRole('button', { name: '模型', exact: true }).click()
    await settings.getByRole('heading', { name: '模型', exact: true }).waitFor()
    expect(await settings.getByText('DeepSeek', { exact: true }).count()).toBe(1)
    await settings.getByRole('button', { name: '关闭', exact: true }).click()

    const warningStart = harness.warnings.length
    await harness.page.reload({ waitUntil: 'load' })
    acknowledgeReloadConnectionLoss(harness, warningStart)
    await welcome.waitFor({ timeout: 15_000 })
    expect(await harness.page.locator('#root').evaluate(root => (root as HTMLElement).inert)).toBe(true)
    harness.assertClean()
    const deviceErrors: string[] = []
    const deviceWarnings: string[] = []
    const deviceContext = await harness.browser.newContext({ ignoreHTTPSErrors: true })
    await deviceContext.addCookies(await harness.page.context().cookies())
    const devicePage = await deviceContext.newPage()
    devicePage.on('pageerror', error => deviceErrors.push(error.message))
    devicePage.on('console', message => {
      if (message.type() === 'warning' || message.type() === 'error') deviceWarnings.push(message.text())
    })
    await devicePage.goto(`${new URL(harness.page.url()).origin}/remote`)
    await devicePage.getByRole('heading', { name: 'This browser is paired' }).waitFor()
    expect(await devicePage.getByText('remote-welcome', { exact: true }).count()).toBe(1)
    await harness.page.evaluate(() => new Promise<void>((resolve, reject) => {
      const socket = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`)
      ;(window as Window & { __remoteRevocationSocket?: WebSocket }).__remoteRevocationSocket = socket
      socket.addEventListener('open', () => resolve(), { once: true })
      socket.addEventListener('error', () => reject(new Error('remote WebSocket failed to open')), { once: true })
    }))
    const shutdownError = await harness.page.evaluate(() => new Promise<string>((resolve, reject) => {
      const socket = (window as Window & { __remoteRevocationSocket?: WebSocket }).__remoteRevocationSocket
      if (socket === undefined) return reject(new Error('remote WebSocket is absent'))
      const timeout = setTimeout(() => reject(new Error('remote shutdown denial timed out')), 3_000)
      socket.addEventListener('message', event => {
        const frame = JSON.parse(String(event.data)) as { requestId?: string, error?: { code?: string } }
        if (frame.requestId !== 'remote-shutdown') return
        clearTimeout(timeout)
        resolve(frame.error?.code ?? '')
      })
      socket.send(JSON.stringify({ requestId: 'remote-shutdown', namespace: 'host', method: 'shutdown', args: {} }))
    }))
    expect(shutdownError).toBe('REMOTE_HOST_DENIED')
    await devicePage.getByRole('button', { name: 'Disconnect this device' }).click()
    await devicePage.getByRole('heading', { name: 'A pairing link is required' }).waitFor()
    expect(deviceErrors).toEqual([])
    expect(deviceWarnings).toEqual([])
    await deviceContext.close()
    await harness.page.evaluate(() => new Promise<void>((resolve, reject) => {
      const socket = (window as Window & { __remoteRevocationSocket?: WebSocket }).__remoteRevocationSocket
      if (socket === undefined) return reject(new Error('remote WebSocket is absent'))
      if (socket.readyState === WebSocket.CLOSED) return resolve()
      const timeout = setTimeout(() => reject(new Error('revoked remote WebSocket stayed open')), 3_000)
      socket.addEventListener('close', () => {
        clearTimeout(timeout)
        resolve()
      }, { once: true })
    }))
    expect(harness.warnings.splice(0)).toEqual(['[web-runtime] connection lost, retry #1'])
    harness.assertClean()
  } finally {
    await harness.close()
  }
})
