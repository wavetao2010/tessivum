import assert from 'node:assert/strict';
import test from 'node:test';

import * as guard from './check-windows-node-unicode-fs.mjs';

const supportedVersionCases = [
  ['22.18.99', false],
  ['22.19.0', true],
  ['22.19.1', true],
  ['22.99.99', true],
  ['23.0.0', false],
  ['24.13.0', false],
  ['24.13.1', true],
  ['24.20.0', true],
  ['24.99.99', true],
  ['25.0.0', false],
  ['20.99.99', false],
  ['26.0.0', false],
  ['99.0.0', false],
  ['24.13', false],
  ['not-a-version', false],
];

for (const [version, expected] of supportedVersionCases) {
  test(`isSupportedWindowsNodeVersion(${version}) returns ${expected}`, () => {
    assert.equal(guard.isSupportedWindowsNodeVersion(version), expected);
  });
}

test('accepts the portable Node 24.20.0 runtime', () => {
  assert.equal(guard.isSupportedWindowsNodeVersion('24.20.0'), true);
});
