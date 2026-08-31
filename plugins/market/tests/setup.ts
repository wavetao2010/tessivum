import { beforeEach } from 'vitest'

const PROXY_ENV = [
  'HTTPS_PROXY', 'https_proxy', 'HTTP_PROXY', 'http_proxy',
  'npm_config_https_proxy', 'npm_config_proxy', 'npm_config_noproxy',
] as const

beforeEach(() => {
  for (const name of PROXY_ENV) delete process.env[name]
})
