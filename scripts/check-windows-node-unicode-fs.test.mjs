import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdtempSync,
  readdirSync,
  rmSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import * as guard from './check-windows-node-unicode-fs.mjs';

const moduleUrl = new URL('./check-windows-node-unicode-fs.mjs', import.meta.url);
const modulePath = fileURLToPath(moduleUrl);
const portableNodePath = String.raw`C:\Users\Q\Documents\New project\.tools\node-v24.20.0-win-x64\node.exe`;
const oldNodePath = String.raw`D:\Program Files\nodejs\node.exe`;
const oldNodeVersionResult = spawnSync(oldNodePath, ['--version'], {
  encoding: 'utf8',
});
const hasAffectedNode = oldNodeVersionResult.status === 0
  && oldNodeVersionResult.stdout.trim() === 'v24.11.1';
const hasCapableCurrentNode = guard.isSupportedWindowsNodeVersion(
  process.versions.node,
);
const hasAffectedCurrentNode = process.platform === 'win32'
  && process.versions.node === '24.11.1';
const testOwnedParents = new Set();

test.afterEach(() => {
  for (const parentDirectory of testOwnedParents) {
    rmSync(parentDirectory, { recursive: true, force: true });
    assert.equal(existsSync(parentDirectory), false);
  }
  testOwnedParents.clear();
});

function createTestOwnedParent() {
  const parentDirectory = mkdtempSync(join(
    tmpdir(),
    'tessivum-node-probe-test-',
  ));
  testOwnedParents.add(parentDirectory);
  return parentDirectory;
}

function assertDiagnosticFacts(text, version = '24.11.1', platform = 'win32') {
  assert.match(text, new RegExp(version.replaceAll('.', String.raw`\.`)));
  assert.match(text, new RegExp(platform));
  assert.match(text, /version-policy/);
  for (const range of guard.SUPPORTED_NODE_RANGES) {
    assert.ok(text.includes(range), `expected diagnostic to contain ${range}`);
  }
  assert.match(text, /https:\/\/github\.com\/nodejs\/node\/issues\/61067/);
}

function unicodeProbeEntries() {
  return readdirSync(tmpdir(), { withFileTypes: true })
    .filter((entry) => entry.name.startsWith('测试 路径-'))
    .map((entry) => entry.name)
    .sort();
}

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

test('rejects an affected Windows Node version with policy diagnostics', () => {
  assert.deepEqual(guard.SUPPORTED_NODE_RANGES, [
    '>=22.19.0 <23.0.0',
    '>=24.13.1 <25.0.0',
  ]);

  assert.throws(
    () => guard.assertSupportedWindowsNodeVersion('24.11.1', 'win32'),
    (error) => {
      assertDiagnosticFacts(error.message);
      return true;
    },
  );
});

test('affected Node CLI rejects before creating probe-owned paths', {
  skip: hasAffectedNode ? false : 'exact Node 24.11.1 runtime is unavailable',
}, () => {
  const before = unicodeProbeEntries();
  const result = spawnSync(oldNodePath, [modulePath], { encoding: 'utf8' });
  const after = unicodeProbeEntries();

  assert.notEqual(result.status, 0);
  assertDiagnosticFacts(`${result.stdout}${result.stderr}`);
  assert.deepEqual(after, before);
});

test('stdin import does not execute the CLI', () => {
  const result = spawnSync(process.execPath, ['--input-type=module'], {
    encoding: 'utf8',
    input: `import ${JSON.stringify(moduleUrl.href)};\n`,
  });

  assert.equal(result.status, 0);
  assert.equal(result.stdout, '');
  assert.equal(result.stderr, '');
});

test('capable Node replaces and validates files under a Unicode path', {
  skip: hasCapableCurrentNode ? false : 'current Node is outside supported ranges',
}, () => {
  const parentDirectory = createTestOwnedParent();
  const result = guard.probeWindowsNodeUnicodeFilesystem({ parentDirectory });

  assert.equal(result.nodeVersion, process.version);
  assert.equal(result.platform, process.platform);
  assert.equal(dirname(result.ownedUnicodeParent), parentDirectory);
  assert.match(basename(result.ownedUnicodeParent), /^测试 路径-[0-9a-f-]+$/);
  assert.equal(result.probeRoot, join(result.ownedUnicodeParent, 'probe-root'));
  assert.equal(result.testedPath, join(result.probeRoot, 'esbuild'));
  assert.deepEqual(result.validatedContents, {
    'package.json': '{"name":"probe-stage"}\n',
    'bin/esbuild.exe': 'probe-binary\n',
    'lib/main.js': 'export const probe = true;\n',
  });
  assert.equal(existsSync(result.probeRoot), false);
  assert.equal(existsSync(result.ownedUnicodeParent), false);
  assert.equal(existsSync(parentDirectory), true);
});

test('portable Node CLI reports one concise success line', {
  skip: hasCapableCurrentNode ? false : 'current Node is outside supported ranges',
}, () => {
  const result = spawnSync(portableNodePath, [modulePath], { encoding: 'utf8' });

  assert.equal(result.status, 0);
  assert.equal(result.stderr, '');
  assert.equal(result.stdout.trim().split(/\r?\n/).length, 1);
  assert.match(result.stdout, /Windows Node Unicode filesystem probe passed/);
  assert.ok(result.stdout.includes(process.version));
  assert.ok(result.stdout.includes(process.platform));
});

test('affected Node preserves failure and removes every probe-owned path', {
  skip: hasAffectedCurrentNode ? false : 'requires Windows Node 24.11.1',
}, () => {
  const parentDirectory = createTestOwnedParent();
  let failure;

  try {
    guard.probeWindowsNodeUnicodeFilesystem({ parentDirectory });
    assert.fail('affected Node unexpectedly completed the filesystem probe');
  } catch (error) {
    failure = error;
  }

  assert.equal(failure.operation, 'remove-target');
  assert.equal(dirname(failure.ownedUnicodeParent), parentDirectory);
  assert.equal(
    failure.probeRoot,
    join(failure.ownedUnicodeParent, 'probe-root'),
  );
  assert.equal(failure.testedPath, join(failure.probeRoot, 'esbuild'));
  assert.match(`${failure.message}\n${failure.details}`, /silent no-op/i);
  assert.equal(failure.cleanupComplete, true);
  assert.equal(existsSync(failure.testedPath), false);
  assert.equal(existsSync(failure.probeRoot), false);
  assert.equal(existsSync(failure.ownedUnicodeParent), false);
  assert.equal(existsSync(parentDirectory), true);
});

test('formats structured capability and aggregate cleanup failures', () => {
  const testedPath = join(
    tmpdir(),
    '测试 路径-formatter',
    'probe-root',
    'esbuild',
  );
  const primary = new Error('rmSync silently left the target in place');
  Object.assign(primary, {
    operation: 'remove-target',
    testedPath,
    probeRoot: dirname(testedPath),
    ownedUnicodeParent: dirname(dirname(testedPath)),
    details: 'silent no-op after rmSync',
    cleanupComplete: false,
  });
  const primaryText = guard.formatWindowsNodeUnicodeFilesystemError(primary);

  assert.ok(primaryText.includes(process.version));
  assert.ok(primaryText.includes(process.platform));
  assert.ok(primaryText.includes(primary.operation));
  assert.ok(primaryText.includes(testedPath));
  assert.ok(primaryText.includes(primary.message));
  assert.ok(primaryText.includes(primary.details));
  assert.ok(primaryText.includes('cleanupComplete: false'));
  for (const range of guard.SUPPORTED_NODE_RANGES) {
    assert.ok(primaryText.includes(range));
  }
  assert.match(primaryText, /https:\/\/github\.com\/nodejs\/node\/issues\/61067/);

  const cleanup = new Error('rmdirSync failed with access denied');
  cleanup.operation = 'cleanup-owned-path';
  cleanup.details = 'bottom-up cleanup could not remove the owned parent';
  const aggregate = new AggregateError(
    [primary, cleanup],
    'filesystem probe failed and cleanup was incomplete',
  );
  Object.assign(aggregate, {
    operation: primary.operation,
    testedPath: primary.testedPath,
    probeRoot: primary.probeRoot,
    ownedUnicodeParent: primary.ownedUnicodeParent,
    cleanupComplete: false,
  });
  const aggregateText = guard.formatWindowsNodeUnicodeFilesystemError(aggregate);

  assert.ok(aggregateText.includes('AggregateError'));
  assert.ok(aggregateText.includes(process.version));
  assert.ok(aggregateText.includes(process.platform));
  assert.ok(aggregateText.includes(primary.operation));
  assert.ok(aggregateText.includes(testedPath));
  assert.ok(aggregateText.includes('cleanupComplete: false'));
  for (const range of guard.SUPPORTED_NODE_RANGES) {
    assert.ok(aggregateText.includes(range));
  }
  assert.match(aggregateText, /https:\/\/github\.com\/nodejs\/node\/issues\/61067/);
  assert.ok(aggregateText.includes(aggregate.message));
  assert.ok(aggregateText.includes(primary.message));
  assert.ok(aggregateText.includes(cleanup.message));
  assert.ok(aggregateText.includes(cleanup.details));
});
