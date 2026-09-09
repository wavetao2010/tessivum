import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'



async function settingsDialog(harness: RustWebHarness) {
  await harness.page.getByRole('button', { name: '设置', exact: true }).click()
  const dialog = harness.page.getByRole('dialog', { name: '设置' })
  await dialog.waitFor({ timeout: 10_000 })
  await dialog.getByRole('button', { name: '模型' }).click()
  return dialog
}

let harness: RustWebHarness | undefined
let discoveryServer: ReturnType<typeof Bun.serve> | undefined


test('models settings configures a dormant provider through the native host', async () => {
  discoveryServer = Bun.serve({
    hostname: '127.0.0.1',
    port: 0,
    fetch(request) {
      if (new URL(request.url).pathname !== '/v1/models') return new Response(null, { status: 404 })
      return Response.json({
        data: [{
          id: 'acme-discovered',
          name: 'Acme Discovered',
          context_window: 65_536,
          max_output_tokens: 8_192,
        }],
      })
    },
  })
  harness = await RustWebHarness.launch({
    name: 'models-settings-web-e2e',
    locale: 'zh-CN',
    env: { ANTHROPIC_API_KEY: '', DEEPSEEK_API_KEY: '', OPENAI_API_KEY: '' },
  })
  try {
    const dialog = await settingsDialog(harness)
    await dialog.getByText('填入各提供方的 API 密钥即可使用其模型。').waitFor({ timeout: 10_000 })
    const add = dialog.getByRole('button', { name: '添加提供方' })
    await add.waitFor({ timeout: 10_000 })
    await waitUntil(() => add.isEnabled(), enabled => enabled, 10_000)
    await add.click()
    const pick = dialog.getByLabel('提供方')
    await pick.waitFor({ timeout: 10_000 })
    await waitUntil(() => pick.locator('option').count(), count => count > 30, 10_000)
    const firstRunKey = dialog.getByPlaceholder('输入 API 密钥', { exact: true })
    if (await firstRunKey.count() !== 0) {
      await dialog.getByRole('button', { name: '取消', exact: true }).first().click()
      await waitUntil(() => firstRunKey.count(), count => count === 0, 10_000)
    }
    const options = await pick.locator('option').allTextContents()
    expect(options).toContain('anthropic')
    expect(options).toContain('minimax-cn')
    await pick.selectOption('minimax-cn')
    await dialog.getByRole('textbox', { name: 'API 密钥', exact: true }).waitFor({ timeout: 10_000 })

    const key = dialog.getByRole('textbox', { name: 'API 密钥', exact: true })
    const save = dialog.getByRole('button', { name: '保存', exact: true })
    await key.fill('sk-😀minimax')
    await dialog.getByText('该 API 密钥格式错误，请检查。').waitFor({ timeout: 10_000 })
    await waitUntil(() => save.isEnabled(), enabled => !enabled, 10_000)
    await key.fill('')
    await waitUntil(() => save.isEnabled(), enabled => enabled, 10_000)
    expect(await dialog.getByText('该 API 密钥格式错误，请检查。').count()).toBe(0)

    const storedProfile = harness.page.waitForResponse(response => new URL(response.url()).pathname === '/api/settings.mutate')
    await save.click()
    expect((await (await storedProfile).json()).result).toMatchObject({
      ok: false, error: { code: 'settings-rejected' },
    })
    const rowCard = (name: string) => dialog.locator('li').filter({ hasText: name }).first()
    await dialog.getByRole('textbox', { name: 'API 密钥', exact: true }).fill('sk-e2e-minimax')
    await dialog.getByRole('button', { name: '保存', exact: true }).click()
    await waitUntil(
      () => dialog.getByRole('textbox', { name: 'API 密钥', exact: true }).count(),
      count => count === 0,
      10_000,
    )
    await dialog.getByRole('img', { name: 'API 密钥已配置' }).waitFor({ timeout: 10_000 })
    await dialog.getByText('已保存 minimax-cn。', { exact: true }).waitFor({ timeout: 10_000 })
    let document = await readFile(join(harness.dataDir, 'settings.yaml'), 'utf8')
    expect(document).toContain('minimax-cn:')
    expect(document).toContain('apiKeyEnv: MINIMAX_CN_API_KEY')
    expect(document).not.toContain('sk-e2e-minimax')
    await waitUntil(
      () => readFile(join(harness.dataDir, 'credentials.yaml'), 'utf8').catch(() => ''),
      document => document.includes('MINIMAX_CN_API_KEY: sk-e2e-minimax'),
      10_000,
    )
    expect(await harness.page.content()).not.toContain('sk-e2e-minimax')

    await dialog.getByRole('button', { name: '编辑 minimax-cn' }).click()
    await dialog.getByText('自定义设置').click()
    const url = dialog.getByLabel('API 地址')
    await url.waitFor({ timeout: 10_000 })
    await url.fill('https://gateway.minimax.example/v1')
    await dialog.getByRole('button', { name: '保存', exact: true }).click()
    await waitUntil(() => dialog.getByLabel('API 地址').count(), count => count === 0, 10_000)
    await dialog.getByText('已保存 minimax-cn。', { exact: true }).waitFor({ timeout: 10_000 })
    document = await readFile(join(harness.dataDir, 'settings.yaml'), 'utf8')
    expect(document).toContain('baseURL: https://gateway.minimax.example/v1')
    expect(document).toContain('apiKeyEnv: MINIMAX_CN_API_KEY')

    const declare = dialog.getByRole('button', { name: '添加自定义提供方' })
    await waitUntil(() => declare.isEnabled(), enabled => enabled, 10_000)
    await declare.click()
    await dialog.getByLabel('Provider ID').fill('acme-gateway')
    await dialog.getByLabel('显示名称').fill('Acme Gateway')
    await dialog.getByLabel('API 地址').fill('https://gateway.acme.example/v1')
    await dialog.getByRole('textbox', { name: 'API 密钥', exact: true }).fill('fixture-acme-key')
    expect(await dialog.getByLabel('推理强度').count()).toBe(0)
    await dialog.getByRole('button', { name: '添加模型' }).click()
    await dialog.getByLabel('模型 ID 1').fill('acme-large')
    await dialog.getByRole('button', { name: '创建提供方', exact: true }).click()
    await dialog.getByText('Acme Gateway', { exact: true }).first().waitFor({ timeout: 10_000 })
    document = await readFile(join(harness.dataDir, 'settings.yaml'), 'utf8')
    expect(document).toContain('acme-gateway:')
    await waitUntil(() => rowCard('Acme Gateway').getByText('自定义').count(), count => count === 1, 10_000)
    expect(await rowCard('minimax-cn').getByText('自定义').count()).toBe(0)

    await dialog.getByRole('button', { name: '编辑 Acme Gateway (acme-gateway)' }).click()
    await dialog.getByText('自定义设置').click()
    const protocol = dialog.getByLabel('API 协议')
    await protocol.waitFor({ timeout: 10_000 })
    expect(await protocol.inputValue()).toBe('openai-completions')
    const name = dialog.getByLabel('显示名称', { exact: true })
    expect(await name.inputValue()).toBe('Acme Gateway')
    await dialog.getByLabel('API 地址').fill(`http://127.0.0.1:${discoveryServer.port}/v1`)
    await dialog.getByRole('button', { name: '获取可用模型' }).click()
    const discovered = harness.page.getByRole('dialog', { name: '选择要添加的模型' })
    await discovered.getByText('acme-discovered', { exact: true }).waitFor({ timeout: 10_000 })
    await discovered.getByRole('button', { name: '添加所选' }).click()
    await dialog.locator('input[value="acme-discovered"]').waitFor({ timeout: 10_000 })
    await protocol.selectOption('anthropic-messages')
    await name.fill('Acme 网关')
    await dialog.getByRole('button', { name: '保存', exact: true }).click()
    await waitUntil(() => dialog.getByLabel('API 协议').count(), count => count === 0, 10_000)
    await dialog.getByText('Acme 网关', { exact: true }).first().waitFor({ timeout: 10_000 })
    await dialog.getByText('已保存 Acme 网关 (acme-gateway)。', { exact: true }).waitFor({ timeout: 10_000 })
    document = await readFile(join(harness.dataDir, 'settings.yaml'), 'utf8')
    expect(document).toContain('api: anthropic-messages')
    expect(document).toContain('displayName: Acme 网关')

    await dialog.getByRole('button', { name: '删除 minimax-cn', exact: true }).click()
    const deleteDialog = harness.page.getByRole('dialog', { name: '删除 minimax-cn？' })
    await deleteDialog.waitFor({ timeout: 10_000 })
    await deleteDialog.getByRole('button', { name: '取消', exact: true }).click()
    expect(await readFile(join(harness.dataDir, 'settings.yaml'), 'utf8')).toContain('minimax-cn:')
    await dialog.getByRole('button', { name: '删除 minimax-cn', exact: true }).click()
    await harness.page.getByRole('dialog', { name: '删除 minimax-cn？' })
      .getByRole('button', { name: '删除 minimax-cn', exact: true }).click()
    await waitUntil(
      () => readFile(join(harness.dataDir, 'settings.yaml'), 'utf8'),
      document => !document.includes('minimax-cn:'),
      10_000,
    )
    expect(await readFile(join(harness.dataDir, 'credentials.yaml'), 'utf8')).not.toContain('MINIMAX_CN_API_KEY')
    await waitUntil(() => harness?.page.getByRole('dialog', { name: '删除 minimax-cn？' }).count() ?? 0, count => count === 0, 10_000)
    await harness.page.keyboard.press('Escape')
    harness.assertClean()
  } finally {
    await harness.close()
    discoveryServer?.stop(true)
    discoveryServer = undefined
    harness = undefined
  }
}, 120_000)

