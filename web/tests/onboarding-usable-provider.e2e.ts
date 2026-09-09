import { mkdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness } from './support'

test('a configured provider remains available without becoming an implicit selection', async () => {
  const harness = await RustWebHarness.launch({
    name: 'neutral-configured-provider-web-e2e',
    locale: 'zh-CN',
    env: { ACME_API_KEY: 'fixture-acme', DEEPSEEK_API_KEY: '' },
    beforeStart: async candidate => {
      await mkdir(candidate.dataDir, { recursive: true })
      await writeFile(join(candidate.dataDir, 'settings.yaml'), [
        'llm-pi-ai:',
        '  providers:',
        '    acme-gateway:',
        '      displayName: Acme Gateway',
        '      baseURL: https://gateway.acme.example/v1',
        '      auth: api-key',
        '      apiKeyEnv: ACME_API_KEY',
        '      models: [{ id: acme-large, name: Acme Large, input: [text] }]',
        '',
      ].join('\n'))
    },
  })
  try {
    const trigger = harness.page.getByRole('button', { name: '选择模型', exact: true })
    await trigger.waitFor({ timeout: 10_000 })
    expect(await harness.page.getByRole('dialog', { name: /内测声明|添加一个 API Key/ }).count()).toBe(0)
    const sessionId = (await harness.sessions()).find(session => session.blank)?.sessionId
    if (sessionId === undefined) throw new Error('fresh app has no blank session')
    expect(await harness.rpc('session.models', { sessionId })).toMatchObject({
      ok: true,
      value: { current: null },
    })

    await trigger.click()
    await harness.page.getByRole('menuitem', { name: /模型/ }).click()
    const acme = harness.page.getByRole('menuitemradio', { name: 'Acme Large' })
    expect(await acme.textContent()).toContain('仅支持文本')
    await acme.click()
    await harness.page.getByRole('button', { name: /^选择模型，当前 Acme Large/ }).waitFor()
    expect(await harness.rpc('session.models', { sessionId })).toMatchObject({
      ok: true,
      value: { current: { provider: 'acme-gateway', model: 'acme-large' } },
    })
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 90_000)
