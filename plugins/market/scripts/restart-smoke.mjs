#!/usr/bin/env node
import assert from 'node:assert/strict'
import * as restart from '../lib/restart.js'

function request(remoteAddress, origin = 'http://127.0.0.1:3080', host = '127.0.0.1:3080') {
  return { socket: { remoteAddress }, headers: { origin, host } }
}

assert.deepEqual(Object.keys(restart), ['trustedDownloadRequest'])
assert.equal(restart.trustedDownloadRequest(request('127.0.0.1')), true)
assert.equal(restart.trustedDownloadRequest(request('::1', 'http://localhost:3080', 'localhost:3080')), true)
assert.equal(restart.trustedDownloadRequest(request('::ffff:127.0.0.1')), true)
assert.equal(restart.trustedDownloadRequest(request('192.168.1.2')), false)
assert.equal(restart.trustedDownloadRequest(request('127.0.0.1', 'http://evil.example')), false)
assert.equal(restart.trustedDownloadRequest(request('127.0.0.1', 'file://127.0.0.1:3080')), false)
assert.equal(restart.trustedDownloadRequest({
  ...request('127.0.0.1'),
  headers: { ...request('127.0.0.1').headers, 'x-forwarded-for': '127.0.0.1' },
}), false)

console.log('restart smoke ok: process control absent and download route guarded')
