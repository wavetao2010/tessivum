import { mkdir, writeFile, readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { expect, test } from 'bun:test'
import { captureStableAria, fixture, RustWebHarness } from './support'

interface SeedSkill {
  name: string
  description: string
  frontmatter: string
}

const SKILLS: readonly SeedSkill[] = [
  { name: 'policy-shared', description: 'Available to both model and user invocation', frontmatter: '' },
  { name: 'policy-model-only', description: 'Available only to model invocation', frontmatter: 'user-invocable: false\n' },
  { name: 'policy-user-only', description: 'Available only to user invocation', frontmatter: 'disable-model-invocation: true\n' },
  { name: 'policy-trusted-only', description: 'Available only to trusted internal callers', frontmatter: 'disable-model-invocation: true\nuser-invocable: false\n' },
]

test('slash suggestions expose every user-invocable skill and label model-hidden entries', async () => {
  const harness = await RustWebHarness.launch({
    name: 'skill-invocation-policy-web-e2e',
    locale: 'en-US',
    beforeStart: async candidate => {
      for (const skill of SKILLS) {
        const directory = join(candidate.workspace, '.agents', 'skills', skill.name)
        await mkdir(directory, { recursive: true })
        const policyLines = skill.frontmatter === '' ? [] : skill.frontmatter.trimEnd().split('\n')
        await writeFile(join(directory, 'SKILL.md'), [
          '---',
          `name: ${skill.name}`,
          `description: ${skill.description}`,
          ...policyLines,
          '---',
          '',
          `# ${skill.name}`,
          '',
        ].join('\n'))
      }
    },
  })
  try {
    const composer = harness.page.locator('textarea:enabled').last()
    await composer.fill('/policy')
    const menu = harness.page.getByRole('listbox', { name: 'Trigger suggestions' })
    await menu.waitFor({ timeout: 15_000 })
    await menu.getByRole('option', { name: /policy-shared/ }).waitFor({ timeout: 15_000 })

    expect(await menu.getByRole('option', { name: /policy-shared/ }).count()).toBe(1)
    expect(await menu.getByRole('option', { name: /policy-user-only user-only · / }).count()).toBe(1)
    expect(await menu.getByRole('option', { name: /policy-model-only/ }).count()).toBe(0)
    expect(await menu.getByRole('option', { name: /policy-trusted-only/ }).count()).toBe(0)
    expect(await captureStableAria(harness.page, '[role="listbox"]')).toBe(
      (await readFile(await fixture('skill-invocation-policy', 'menu.expected.md'), 'utf8')).trim(),
    )
    harness.assertClean()
  } finally {
    await harness.close()
  }
}, 120_000)
