import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { RustWebHarness, waitUntil } from './support'

test('offers declared model reasoning levels and persists the selected effort', async () => {
  const harness = await RustWebHarness.launch({
    name: 'declared-reasoning-web-e2e',
    locale: 'zh-CN',
    env: { ACME_API_KEY: 'fixture-key' },
    beforeStart: async candidate => {
      await mkdir(candidate.dataDir, { recursive: true })
      await writeFile(join(candidate.dataDir, 'settings.yaml'), [
        'llm-pi-ai:',
        '  providers:',
        '    acme-gateway:',
        '      displayName: Acme Gateway',
        '      baseURL: https://gateway.acme.example/v1',
        '      apiKeyEnv: ACME_API_KEY',
        '      models:',
        '        - id: acme-think',
        '          name: Acme Think',
        '          reasoningEfforts:',
        '            low: low',
        '            medium: medium',
        '            high: high',
        'agent-default-model:',
        '  provider: acme-gateway',
        '  model: acme-think',
        '',
      ].join('\n'))
    },
  })
  try {
    const trigger = harness.page.getByRole('button', { name: /^选择模型/ })
    await trigger.waitFor({ timeout: 15_000 })
    await expect(waitUntil(() => trigger.getAttribute('aria-label'), label => label?.includes('Acme Think') === true))
      .resolves.toContain('Acme Think')
    await trigger.click()
    await harness.page.getByRole('menuitem', { name: /推理等级/ }).click()
    const efforts = harness.page.getByRole('menuitemradio')
    await expect(efforts.allTextContents()).resolves.toEqual(['Default', 'Low', 'Medium', 'High'])
    await harness.page.getByRole('menuitemradio', { name: 'High' }).click()
    await expect(waitUntil(
      () => readFile(join(harness.dataDir, 'settings.yaml'), 'utf8'),
      document => document.includes('reasoningEffort: high'),
    )).resolves.toContain('reasoningEffort: high')
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 60_000)
