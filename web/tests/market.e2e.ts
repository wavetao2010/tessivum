import { afterAll, expect, test } from 'bun:test'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { basename, join } from 'node:path'
import { RustWebHarness } from './support'

const MARKET_ROOT = join(import.meta.dir, '../../plugins/market')
const PROJECT_ROOT = join(MARKET_ROOT, '../../..')
let harness: RustWebHarness | undefined
let packRoot: string | undefined

test('first-party market runs in the native host', async () => {
  const build = Bun.spawn(['bun', 'run', 'build'], { cwd: MARKET_ROOT, stdout: 'inherit', stderr: 'inherit' })
  expect(await build.exited).toBe(0)
  packRoot = await mkdtemp(join(tmpdir(), 'tessivum-market-pack-'))
  const pack = Bun.spawn(['bun', 'pm', 'pack', '--ignore-scripts', '--destination', packRoot], { cwd: MARKET_ROOT, stdout: 'inherit', stderr: 'inherit' })
  expect(await pack.exited).toBe(0)

  const manifest = await Bun.file(join(MARKET_ROOT, 'package.json')).json() as { version: string }
  const tarball = join(packRoot, `tessivum-market-${manifest.version}.tgz`)
  const sha256 = new Bun.CryptoHasher('sha256').update(await Bun.file(tarball).arrayBuffer()).digest('hex')
  const env: Record<string, string> = {
    TESSIVUM_CORE_SOURCE: process.env.TESSIVUM_CORE_SOURCE ?? join(PROJECT_ROOT, 'tessivum-core'),
    TESSIVUM_CORDIS_SOURCE: process.env.TESSIVUM_CORDIS_SOURCE ?? join(PROJECT_ROOT, 'upstream/cordis'),
    TESSIVUM_DEEPSEEK_SOURCE: process.env.TESSIVUM_DEEPSEEK_SOURCE ?? join(PROJECT_ROOT, 'upstream/deepseek-harness'),
    TESSIVUM_DEEPSEEK_VENDOR: process.env.TESSIVUM_DEEPSEEK_VENDOR ?? join(PROJECT_ROOT, 'upstream/deepseek-harness/vendor'),
    TESSIVUM_COMPAT_HOST: process.env.TESSIVUM_COMPAT_HOST ?? join(process.env.TESSIVUM_CORE_SOURCE ?? join(PROJECT_ROOT, 'tessivum-core'), 'node/compat-host/src/index.ts'),
    CORDIS_VENDOR_ROOT: process.env.CORDIS_VENDOR_ROOT ?? process.env.TESSIVUM_DEEPSEEK_VENDOR ?? join(PROJECT_ROOT, 'upstream/deepseek-harness/vendor'),
  }

  harness = await RustWebHarness.launch({
    name: 'market',
    locale: 'zh-CN',
    env,
    async beforeStart(instance) {
      const checksum = join(instance.root, 'market.sha256')
      await writeFile(checksum, `${sha256}  ${basename(tarball)}\n`)
      env.TESSIVUM_MARKET_TARBALL = tarball
      env.TESSIVUM_MARKET_SHA256_FILE = checksum
    },
  })

  const installed = JSON.parse(await readFile(join(harness.dataDir, 'plugins/package.json'), 'utf8')) as {
    dependencies?: Record<string, string>
    dsh?: { profile?: { bundles?: string[] } }
  }
  expect(installed.dependencies?.['tessivum-market']).toContain('/artifacts/market/')
  expect(installed.dsh?.profile?.bundles).toContain('tessivum-market')

  await harness.page.getByRole('button', { name: '设置', exact: true }).click()
  await harness.page.getByRole('button', { name: '插件市场', exact: true }).click()
  await harness.page.getByRole('heading', { name: '插件市场', exact: true }).waitFor()
  await harness.page.getByPlaceholder('搜索插件，例如：通知、终端、记忆…').waitFor()

  await harness.page.getByRole('button', { name: '界面与主题', exact: true }).click()
  await harness.page.getByText('dsh-task-board', { exact: true }).first().waitFor()

  await harness.page.getByRole('button', { name: '主题', exact: true }).first().click()
  await harness.page.getByText(/\d+ 款主题$/).waitFor()
  await harness.page.getByText('dsh-catppuccin-theme', { exact: true }).first().waitFor()

  const snapshot = await harness.page.evaluate(async () => {
    const response = await fetch('/dsh-market/snapshots', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: '{}',
    })
    return { status: response.status, body: await response.json() as { snapshot?: { id?: string } } }
  })
  expect(snapshot.status).toBe(200)
  const snapshotId = snapshot.body.snapshot?.id
  expect(typeof snapshotId).toBe('string')

  const stateFile = join(harness.dataDir, 'plugins/.dsh-market/state.json')
  await writeFile(stateFile, JSON.stringify({
    disabled: ['fixture-disabled'],
    groups: { e2e: ['fixture-disabled'] },
    groupOrder: ['e2e'],
    region: 'global',
    regionAuto: false,
  }))

  const restored = await harness.page.evaluate(async (id) => {
    const response = await fetch('/dsh-market/restore-snapshot', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ snapshot: id }),
    })
    return { status: response.status, body: await response.json() as { ok?: boolean } }
  }, snapshotId!)
  expect(restored).toEqual({ status: 200, body: expect.objectContaining({ ok: true }) })
  expect(JSON.parse(await readFile(stateFile, 'utf8'))).toMatchObject({
    disabled: [],
    groups: {},
    groupOrder: [],
  })
  expect(harness.pageErrors).toEqual([])
}, 300_000)

afterAll(async () => {
  await harness?.close()
  if (packRoot !== undefined) await rm(packRoot, { recursive: true, force: true })
})
