import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readlinkSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import * as guard from './check-windows-node-unicode-fs.mjs';

const moduleUrl = new URL('./check-windows-node-unicode-fs.mjs', import.meta.url);
const modulePath = fileURLToPath(moduleUrl);
const repoRoot = fileURLToPath(new URL('..', import.meta.url));
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

function directoryEntryType(entry) {
  if (entry.isDirectory()) return 'directory';
  if (entry.isFile()) return 'file';
  if (entry.isSymbolicLink()) return 'symbolic-link';
  if (entry.isBlockDevice()) return 'block-device';
  if (entry.isCharacterDevice()) return 'character-device';
  if (entry.isFIFO()) return 'fifo';
  if (entry.isSocket()) return 'socket';
  return 'unknown';
}

function compareText(left, right) {
  if (left < right) return -1;
  if (left > right) return 1;
  return 0;
}

function sectionBetween(source, startToken, endToken, label) {
  const start = source.indexOf(startToken);
  assert.notEqual(start, -1, `${label}: missing start token: ${startToken}`);
  const end = source.indexOf(endToken, start + startToken.length);
  assert.notEqual(end, -1, `${label}: missing end token: ${endToken}`);
  return source.slice(start, end);
}

function assertTokensInOrder(source, tokens, label) {
  let previousIndex = -1;
  for (const token of tokens) {
    const index = source.indexOf(token, previousIndex + 1);
    assert.notEqual(
      index,
      -1,
      `${label}: missing or out-of-order token: ${token}`,
    );
    previousIndex = index;
  }
}

function snapshotUnicodeProbeTrees(root) {
  const snapshot = [];

  function visit(entry, relativePath) {
    const absolutePath = join(root, ...relativePath.split('/'));
    const type = directoryEntryType(entry);
    const record = { path: relativePath, type };

    if (type === 'file') {
      record.sha256 = createHash('sha256')
        .update(readFileSync(absolutePath))
        .digest('hex');
    } else if (type === 'symbolic-link') {
      record.target = readlinkSync(absolutePath);
    }
    snapshot.push(record);

    if (type === 'directory') {
      const children = readdirSync(absolutePath, { withFileTypes: true })
        .sort((left, right) => compareText(left.name, right.name));
      for (const child of children) {
        visit(child, `${relativePath}/${child.name}`);
      }
    }
  }

  const ownedRoots = readdirSync(root, { withFileTypes: true })
    .filter((entry) => entry.name.startsWith('测试 路径-'))
    .sort((left, right) => compareText(left.name, right.name));
  for (const ownedRoot of ownedRoots) {
    visit(ownedRoot, ownedRoot.name);
  }

  return snapshot.sort((left, right) => compareText(left.path, right.path));
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

test('Windows CI gates external dependencies on the Node prerequisite', () => {
  const workflow = readFileSync(
    join(repoRoot, '.github', 'workflows', 'ci.yml'),
    'utf8',
  );
  const windowsJob = sectionBetween(
    workflow,
    '\n  windows:',
    '\n  browser-e2e:',
    'Windows CI job',
  );

  assertTokensInOrder(windowsJob, [
    'actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09',
    'actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38',
    'node-version: 24.20.0',
    'node --test scripts/check-windows-node-unicode-fs.test.mjs',
    'node scripts/check-windows-node-unicode-fs.mjs',
    'repository: deepseek-ai/deepseek-harness',
    'repository: cordiverse/cordis',
    'repository: wavetao2010/tessivum-core',
    'dtolnay/rust-toolchain@032958afbdc797a9164d3bc0b56325c1308924a5',
    'oven-sh/setup-bun@0c5077e51419868618aeaa5fe8019c62421857d6',
    'pnpm/action-setup@b906affcce14559ad1aafd4ab0e942779e9f58b1',
    'name: Install pinned DeepSeek build dependencies',
  ], 'Windows CI prerequisite order');
});

test('Windows source guide gates external dependencies on Node support', () => {
  const guide = readFileSync(
    join(repoRoot, 'docs', 'WINDOWS_SOURCE_TEST.md'),
    'utf8',
  );
  assert.ok(guide.includes('>=22.19.0 <23.0.0'));
  assert.ok(guide.includes('>=24.13.1 <25.0.0'));
  assert.ok(guide.includes('Node 24.20.0'));

  const mainScript = sectionBetween(
    guide,
    '```powershell\n#Requires -Version 7.4',
    '\n```',
    'Windows source guide main PowerShell block',
  );
  assertTokensInOrder(mainScript, [
    '$Node = Get-Command node -ErrorAction Stop',
    'git clone https://github.com/wavetao2010/tessivum.git $Repo',
    '& $Node.Source --version',
    '& $Node.Source scripts/check-windows-node-unicode-fs.mjs',
    'git clone https://github.com/deepseek-ai/deepseek-harness.git',
    'git clone https://github.com/cordiverse/cordis.git',
    'git clone https://github.com/wavetao2010/tessivum-core.git',
    '$Pnpm = Get-Command pnpm -ErrorAction Stop',
    'pnpm --version',
    'pnpm install --frozen-lockfile',
  ], 'Windows source guide prerequisite order');

  const probeIndex = mainScript.indexOf(
    '& $Node.Source scripts/check-windows-node-unicode-fs.mjs',
  );
  const pnpmInvocationIndex = mainScript.search(
    /^\s*(?:&\s+(?:\$Pnpm(?:\.Source)?|pnpm)|pnpm)\s+/m,
  );
  assert.ok(
    pnpmInvocationIndex > probeIndex,
    'Windows source guide invokes pnpm before the Node filesystem probe',
  );
});

test('filesystem identity comparison preserves BigInt precision', () => {
  const first = { dev: 9_007_199_254_740_992n, ino: 41n };
  const second = { dev: 9_007_199_254_740_993n, ino: 41n };

  assert.equal(Number(first.dev), Number(second.dev));
  assert.equal(guard.isSameFilesystemIdentity(first, second), false);
  assert.equal(guard.isSameFilesystemIdentity(first, { ...first }), true);
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
  const childTempRoot = createTestOwnedParent();
  const before = snapshotUnicodeProbeTrees(childTempRoot);
  const result = spawnSync(oldNodePath, [modulePath], {
    encoding: 'utf8',
    env: {
      ...process.env,
      TEMP: childTempRoot,
      TMP: childTempRoot,
    },
  });
  const after = snapshotUnicodeProbeTrees(childTempRoot);

  assert.notEqual(result.status, 0);
  assertDiagnosticFacts(`${result.stdout}${result.stderr}`);
  assert.deepEqual(after, before);
  assert.equal(existsSync(childTempRoot), true);
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

test('capable Node CLI reports one concise success line', {
  skip: hasCapableCurrentNode ? false : 'current Node is outside supported ranges',
}, () => {
  const result = spawnSync(process.execPath, [modulePath], { encoding: 'utf8' });

  assert.equal(result.status, 0);
  assert.equal(result.stderr, '');
  assert.equal(result.stdout.trim().split(/\r?\n/).length, 1);
  assert.match(result.stdout, /Windows Node Unicode filesystem probe passed/);
  assert.ok(result.stdout.includes(process.version));
  assert.ok(result.stdout.includes(process.platform));
});

test('does not claim cleanup when owned-path state is uncertain', () => {
  const uncertainParent = join(tmpdir(), 'caller\0invalid');
  let failure;

  try {
    guard.probeWindowsNodeUnicodeFilesystem({
      parentDirectory: uncertainParent,
    });
    assert.fail('probe unexpectedly accepted a NUL-containing parent path');
  } catch (error) {
    failure = error;
  }

  assert.ok(failure instanceof AggregateError);
  assert.equal(failure.operation, 'prepare-probe');
  assert.equal(failure.cleanupComplete, false);
  assert.equal(dirname(failure.ownedUnicodeParent), uncertainParent);
  assert.ok(
    [...failure.errors].some((error) => `${error.message}\n${error.details}`
      .includes('ERR_INVALID_ARG_VALUE')),
  );
});

test('classifies expected-file read failures as content validation', {
  skip: process.platform === 'win32' && hasCapableCurrentNode
    ? false
    : 'requires Windows ACLs on a capable Node runtime',
}, (t) => {
  const parentDirectory = createTestOwnedParent();
  const sentinelPath = join(parentDirectory, 'caller-sentinel.txt');
  const identity = `${process.env.USERDOMAIN}\\${process.env.USERNAME}`;
  writeFileSync(sentinelPath, 'caller-owned\n');
  const denyRead = spawnSync('icacls.exe', [
    parentDirectory,
    '/deny',
    `${identity}:(OI)(IO)(RD)`,
  ], { encoding: 'utf8' });
  if (denyRead.status !== 0) {
    t.skip(`could not establish test ACL: ${denyRead.stderr}`);
    return;
  }

  let failure;
  let restoreAcl;
  try {
    try {
      guard.probeWindowsNodeUnicodeFilesystem({ parentDirectory });
      assert.fail('probe unexpectedly read an ACL-protected stage file');
    } catch (error) {
      failure = error;
    }
  } finally {
    restoreAcl = spawnSync('icacls.exe', [
      parentDirectory,
      '/remove:d',
      identity,
    ], { encoding: 'utf8' });
  }

  assert.equal(restoreAcl.status, 0, restoreAcl.stderr);
  assert.equal(failure.operation, 'validate-content');
  assert.equal(
    failure.failingPath,
    join(failure.testedPath, 'package.json'),
  );
  assert.match(failure.cause.code, /EACCES|EPERM/);
  assert.ok(failure.details.includes(failure.failingPath));
  assert.equal(failure.cleanupComplete, true);
  assert.equal(existsSync(failure.ownedUnicodeParent), false);
  assert.equal(readFileSync(sentinelPath, 'utf8'), 'caller-owned\n');
  assert.equal(existsSync(parentDirectory), true);
});

test('rejects a junction pre-positioned at the probe-owned path', {
  skip: process.platform === 'win32' ? false : 'requires Windows junctions',
}, () => {
  const parentDirectory = createTestOwnedParent();
  const outsideDirectory = join(parentDirectory, 'outside-owned-test-area');
  const sentinelPath = join(outsideDirectory, 'sibling-sentinel.txt');
  const fixedUuid = '00000000-0000-4000-8000-000000000001';
  const predictedOwnedPath = join(parentDirectory, `测试 路径-${fixedUuid}`);
  mkdirSync(outsideDirectory);
  writeFileSync(sentinelPath, 'outside-sentinel\n');
  symlinkSync(outsideDirectory, predictedOwnedPath, 'junction');

  const childScript = String.raw`
    import assert from 'node:assert/strict';
    import crypto from 'node:crypto';
    import { existsSync, lstatSync, readFileSync } from 'node:fs';
    import { syncBuiltinESMExports } from 'node:module';
    import { join } from 'node:path';

    const uuids = [
      '00000000-0000-4000-8000-000000000001',
      '00000000-0000-4000-8000-000000000002',
    ];
    crypto.randomUUID = () => uuids.shift();
    syncBuiltinESMExports();
    const guard = await import(process.env.TEST_MODULE_URL);
    let failure;
    try {
      guard.probeWindowsNodeUnicodeFilesystem({
        parentDirectory: process.env.TEST_PARENT_DIRECTORY,
      });
      assert.fail('probe unexpectedly accepted the pre-positioned junction');
    } catch (error) {
      failure = error;
    }

    assert.equal(failure.operation, 'prepare-probe');
    assert.equal(lstatSync(process.env.TEST_OWNED_PATH).isSymbolicLink(), true);
    assert.equal(
      readFileSync(process.env.TEST_SENTINEL_PATH, 'utf8'),
      'outside-sentinel\n',
    );
    assert.equal(
      existsSync(join(process.env.TEST_OUTSIDE_DIRECTORY, 'probe-root')),
      false,
    );
  `;
  const result = spawnSync(process.execPath, ['--input-type=module'], {
    encoding: 'utf8',
    env: {
      ...process.env,
      TEST_MODULE_URL: moduleUrl.href,
      TEST_PARENT_DIRECTORY: parentDirectory,
      TEST_OWNED_PATH: predictedOwnedPath,
      TEST_OUTSIDE_DIRECTORY: outsideDirectory,
      TEST_SENTINEL_PATH: sentinelPath,
    },
    input: childScript,
  });

  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, '');
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

test('formats cyclic and repeated error references finitely', () => {
  const repeated = new Error('repeated cleanup failure');
  repeated.cause = repeated;
  const aggregate = new AggregateError(
    [repeated, repeated],
    'cyclic aggregate failure',
  );
  Object.assign(aggregate, {
    operation: 'cleanup-owned-path',
    testedPath: join(tmpdir(), 'cycle-test', 'esbuild'),
    cleanupComplete: false,
  });

  const formatted = guard.formatWindowsNodeUnicodeFilesystemError(aggregate);

  assert.ok(formatted.includes('cyclic aggregate failure'));
  assert.ok(formatted.includes('repeated cleanup failure'));
  assert.ok(formatted.includes('[already reported]'));
  assert.ok(formatted.length < 10_000);
});
