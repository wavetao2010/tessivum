import { expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'
import { join } from 'node:path'

const DIST_ROOT = join(import.meta.dir, '../dist')

test('ships install metadata with the built web application', async () => {
  const index = await readFile(join(DIST_ROOT, 'index.html'), 'utf8')
  expect(index).toContain('<link rel="manifest" href="/manifest.webmanifest" />')

  expect(JSON.parse(await readFile(join(DIST_ROOT, 'manifest.webmanifest'), 'utf8'))).toEqual({
    id: '/',
    name: 'Tessivum',
    short_name: 'Tessivum',
    start_url: '/',
    scope: '/',
    display: 'fullscreen',
    icons: [{
      src: '/favicon.svg',
      sizes: 'any',
      type: 'image/svg+xml',
      purpose: 'any',
    }],
  })
})

test('ships the Tessivum mark in the current system color', async () => {
  const favicon = await readFile(join(DIST_ROOT, 'favicon.svg'), 'utf8')
  expect(favicon).toContain('fill="currentColor"')
  expect(favicon).toContain('color-scheme: light dark')
  expect(favicon.match(/<path /g)).toHaveLength(4)
})
