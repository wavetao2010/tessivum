import { afterAll, beforeAll, expect, test } from 'bun:test'
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { RustWebHarness } from './support'

let harness: RustWebHarness
let ambient: string

async function writeSkill(root: string, name: string): Promise<void> {
  const bundle = join(root, name)
  await mkdir(bundle, { recursive: true })
  await writeFile(join(bundle, 'SKILL.md'), `---\nname: ${name}\ndescription: Skill discovery probe\n---\n`)
}

beforeAll(async () => {
  ambient = await mkdtemp(join(tmpdir(), 'tessivum-web-ambient-skills-'))
  const dshHome = join(ambient, 'dsh-home')
  const agentsHome = join(ambient, 'agents-home')
  const bundled = join(ambient, 'bundled')
  await Promise.all([
    writeSkill(join(dshHome, 'skills'), 'ambient-dsh'),
    writeSkill(join(agentsHome, 'skills'), 'ambient-agents'),
    writeSkill(bundled, 'ambient-bundled'),
  ])
  harness = await RustWebHarness.launch({
    name: 'scaffold-hermetic',
    env: { DSH_HOME: dshHome, DSH_AGENTS_HOME: agentsHome, DSH_BUNDLED_SKILL_DIR: bundled },
    beforeStart: candidate => writeSkill(join(candidate.workspace, '.agents/skills'), 'workspace-only'),
  })
}, 120_000)

afterAll(async () => {
  await harness?.close()
  if (ambient !== undefined) await rm(ambient, { recursive: true, force: true })
})

test('isolates Web skill discovery from every ambient host root', async () => {
  const session = (await harness.sessions()).find(item => item.cwd === harness.workspace)
  if (session === undefined) throw new Error('native host created no workspace session')
  const result = await harness.rpc<{ skills: Array<{ name: string }> }>('skill.list', { sessionId: session.sessionId })
  expect(result.ok).toBe(true)
  const names = result.value?.skills.map(skill => skill.name) ?? []
  expect(names).not.toContain('ambient-dsh')
  expect(names).not.toContain('ambient-agents')
  expect(names).not.toContain('ambient-bundled')
  harness.assertClean()
}, 60_000)
