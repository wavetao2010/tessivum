import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

const origin = {
  displayName: 'Origin Gateway', baseURL: 'https://gateway.origin.example/v1', apiKeyEnv: 'ORIGIN_API_KEY',
  models: [{ id: 'origin-large', name: 'Origin Large' }],
}

test('uses a composer model switch as the default without rewriting logged sessions', async () => {
  const harness = await RustWebHarness.launch({
    name: 'default-model-web-e2e',
    locale: 'zh-CN',
    env: { ORIGIN_API_KEY: 'fixture-origin', ACME_API_KEY: 'fixture-acme' },
    beforeStart: async candidate => {
      await mkdir(candidate.dataDir, { recursive: true })
      await writeFile(join(candidate.dataDir, 'settings.yaml'), [
        'llm-pi-ai:',
        '  providers:',
        '    origin-gateway:',
        '      displayName: Origin Gateway',
        '      baseURL: https://gateway.origin.example/v1',
        '      apiKeyEnv: ORIGIN_API_KEY',
        '      models: [{ id: origin-large, name: Origin Large }]',
        '    acme-gateway:',
        '      displayName: Acme Gateway',
        '      baseURL: https://gateway.acme.example/v1',
        '      apiKeyEnv: ACME_API_KEY',
        '      models: [{ id: acme-large, name: Acme Large }]',
        'agent-default-model:',
        '  provider: origin-gateway',
        '  model: origin-large',
        '',
      ].join('\n'))
    },
  })
  try {
    const workspace = await harness.rpc<{ items: Array<{ workspaceId: string }> }>('workspace.list')
    const workspaceId = workspace.value?.items[0]?.workspaceId
    if (workspaceId === undefined) throw new Error('native host has no workspace')
    const create = async (sessionId: string): Promise<string> => {
      const result = await harness.rpc<{ sessionId: string }>('session.create', { sessionId, workspaceId })
      if (!result.ok || result.value === undefined) throw new Error(`session.create failed: ${JSON.stringify(result.error)}`)
      return result.value.sessionId
    }
    const current = async (sessionId: string): Promise<unknown> => {
      const result = await harness.rpc<{ current: unknown }>('session.models', { sessionId })
      if (!result.ok || result.value === undefined) throw new Error(`session.models failed: ${JSON.stringify(result.error)}`)
      return result.value.current
    }

    const logged = await create('default-model-logged')
    expect((await harness.rpc('session.selectModel', {
      sessionId: logged, provider: 'origin-gateway', model: 'origin-large',
    })).ok).toBe(true)

    const trigger = harness.page.getByRole('button', { name: /^选择模型/ })
    await trigger.click()
    await harness.page.getByRole('menuitem', { name: /模型/ }).click()
    await harness.page.getByRole('menuitemradio', { name: 'Acme Large' }).click()
    await expect(waitUntil(
      () => readFile(join(harness.dataDir, 'settings.yaml'), 'utf8'),
      document => document.includes('provider: acme-gateway'),
    )).resolves.toContain('model: acme-large')

    expect(await current(await create('default-model-after'))).toEqual({ provider: 'acme-gateway', model: 'acme-large' })
    expect(await current(logged)).toEqual({ provider: 'origin-gateway', model: 'origin-large' })

    const replaced = await harness.rpc('settings.replace', {
      ns: 'llm-pi-ai', section: { providers: { 'origin-gateway': origin } },
    })
    expect(replaced.ok).toBe(true)
    const box = harness.page.locator('textarea[data-input-phase], textarea').first()
    await expect(waitUntil(() => box.isEnabled(), enabled => !enabled)).resolves.toBe(false)
    expect(await box.getAttribute('placeholder')).toBe('当前模型不可用，请先选择模型')
    const refused = await harness.rpc('session.prompt', {
      sessionId: await create('default-model-refusal'), mode: 'queue', content: [{ type: 'text', text: 'hi' }],
    })
    expect(refused).toMatchObject({ ok: false, error: { code: 'model-unavailable' } })

    await trigger.click()
    await harness.page.getByRole('menuitem', { name: /模型/ }).click()
    await harness.page.getByRole('menuitemradio', { name: 'Origin Large' }).click()
    await expect(waitUntil(() => box.isEnabled(), Boolean)).resolves.toBe(true)
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 90_000)
