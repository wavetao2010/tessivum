import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { join } from 'node:path'

export const inject = ['webServer', 'webRuntime']

async function readState(statePath) {
  return JSON.parse(await readFile(statePath, 'utf8').catch(error => {
    if (error.code === 'ENOENT') return '{}'
    throw error
  }))
}

export function apply(ctx) {
  const home = process.env.DSH_HOME
  if (!home) throw new Error('DSH_HOME is required')
  const statePath = join(home, 'dream-skin.json')
  return ctx.webServer.register({
    kind: 'prefix',
    path: '/dream-skin/api',
    async handler(request, response) {
      response.setHeader('content-type', 'application/json')
      if (request.method !== 'POST') {
        response.statusCode = 405
        response.end(JSON.stringify({ error: 'method not allowed' }))
        return
      }
      const chunks = []
      for await (const chunk of request) chunks.push(chunk)
      let command
      try {
        command = JSON.parse(Buffer.concat(chunks).toString('utf8'))
      } catch {
        response.statusCode = 400
        response.end(JSON.stringify({ error: 'invalid request' }))
        return
      }
      if (command?.method === 'get') {
        response.end(JSON.stringify({ ok: true, value: await readState(statePath) }))
        return
      }
      if (command?.method !== 'set' || command.patch === null || typeof command.patch !== 'object' || Array.isArray(command.patch)) {
        response.statusCode = 400
        response.end(JSON.stringify({ error: 'invalid request' }))
        return
      }
      const state = await readState(statePath)
      for (const [key, value] of Object.entries(command.patch)) {
        if (typeof value === 'string') state[key] = value
        else if (value === null) delete state[key]
        else {
          response.statusCode = 400
          response.end(JSON.stringify({ error: 'invalid request' }))
          return
        }
      }
      await mkdir(home, { recursive: true })
      await writeFile(statePath, JSON.stringify(state))
      response.end(JSON.stringify({ ok: true }))
    },
  })
}
