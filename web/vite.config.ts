import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'
import { createDeepSeekSourceResolver, deepSeekSourcePlugin } from './scripts/deepseek-source-resolver.mjs'

const local = (path: string): string => fileURLToPath(new URL(path, import.meta.url))
const upstreamRoot = process.env.TESSIVUM_DEEPSEEK_SOURCE ?? local('../../upstream/deepseek-harness')
const requireFromWeb = createRequire(import.meta.url)
const upstreamSource = createDeepSeekSourceResolver(upstreamRoot)

const upstreamDependencies = deepSeekSourcePlugin(upstreamSource, requireFromWeb.resolve)

export default defineConfig(({ command }) => {
  if (command === 'serve') {
    throw new Error('tessivum/web is not a standalone application; run `cargo run -- web`. The Rust host supplies window.__DSH_BOOT__.')
  }
  return {
  plugins: [upstreamDependencies, react()],
  build: { sourcemap: true },
  resolve: {
    dedupe: ['react', 'react-dom', '@deepseek-ai/cordis'],
    alias: [
      { find: /^node:module$/, replacement: local('./src/node-module-stub.ts') },
    ],
  },
  define: {
    'process.versions.node': JSON.stringify('0.0.0'),
    'process.execArgv': '[]',
    'process.env.CORDIS_SHARED': 'undefined',
  },
  }
})
