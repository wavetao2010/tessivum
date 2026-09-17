import { randomUUID } from 'node:crypto';
import {
  lstatSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  realpathSync,
  renameSync,
  rmSync,
  rmdirSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import {
  dirname,
  isAbsolute,
  join,
  relative,
  resolve,
  sep,
} from 'node:path';
import { pathToFileURL } from 'node:url';

export const SUPPORTED_NODE_RANGES = Object.freeze([
  '>=22.19.0 <23.0.0',
  '>=24.13.1 <25.0.0',
]);

const NODE_ISSUE_URL = 'https://github.com/nodejs/node/issues/61067';
const STAGE_CONTENTS = Object.freeze({
  'package.json': '{"name":"probe-stage"}\n',
  'bin/esbuild.exe': 'probe-binary\n',
  'lib/main.js': 'export const probe = true;\n',
});

export function isSupportedWindowsNodeVersion(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (!match) {
    return false;
  }

  const major = Number(match[1]);
  const minor = Number(match[2]);
  const patch = Number(match[3]);

  if (major === 22) {
    return minor > 19 || (minor === 19 && patch >= 0);
  }

  if (major === 24) {
    return minor > 13 || (minor === 13 && patch >= 1);
  }

  return false;
}

export function assertSupportedWindowsNodeVersion(
  version = process.versions.node,
  platform = process.platform,
) {
  if (isSupportedWindowsNodeVersion(version)) {
    return;
  }

  const error = new Error([
    'Windows Node Unicode filesystem prerequisite failed.',
    'operation: version-policy',
    `Node version: ${version}`,
    `platform: ${platform}`,
    `Supported Node ranges: ${SUPPORTED_NODE_RANGES.join(', ')}`,
    `Known Node issue: ${NODE_ISSUE_URL}`,
  ].join('\n'));
  error.operation = 'version-policy';
  throw error;
}

function createFilesystemError(operation, details, paths, cause) {
  const error = new Error(`${operation}: ${details}`, cause === undefined
    ? undefined
    : { cause });
  error.operation = operation;
  error.testedPath = paths.testedPath;
  error.probeRoot = paths.probeRoot;
  error.ownedUnicodeParent = paths.ownedUnicodeParent;
  error.stagePath = paths.stagePath;
  error.details = details;
  return error;
}

function writeContents(root, contents) {
  for (const [relativePath, contentsText] of Object.entries(contents)) {
    const destination = join(root, ...relativePath.split('/'));
    mkdirSync(dirname(destination), { recursive: true });
    writeFileSync(destination, contentsText);
  }
}

function inspectPath(path) {
  try {
    return { status: 'present', stats: lstatSync(path, { bigint: true }) };
  } catch (error) {
    if (error.code === 'ENOENT') {
      return { status: 'absent' };
    }
    return { status: 'unknown', error };
  }
}

export function isSameFilesystemIdentity(left, right) {
  return left.dev === right.dev && left.ino === right.ino;
}

function pathIsWithin(root, candidate, allowRoot = true) {
  const difference = relative(root, candidate);
  if (difference === '') {
    return allowRoot;
  }
  return difference !== '..'
    && !difference.startsWith(`..${sep}`)
    && !isAbsolute(difference);
}

function verifyDirectory(directory, expectedIdentity, expectedRealPath, ownedRoot) {
  const state = inspectPath(directory);
  if (state.status === 'unknown') {
    throw state.error;
  }
  if (state.status === 'absent') {
    throw new Error(`directory disappeared before cleanup: ${directory}`);
  }
  if (state.stats.isSymbolicLink() || !state.stats.isDirectory()) {
    throw new Error(`refusing to traverse a non-directory or reparse alias: ${directory}`);
  }

  const identity = { dev: state.stats.dev, ino: state.stats.ino };
  if (!isSameFilesystemIdentity(identity, expectedIdentity)) {
    throw new Error(`directory identity changed before cleanup: ${directory}`);
  }
  const realPath = realpathSync.native(directory);
  if (relative(expectedRealPath, realPath) !== '') {
    throw new Error(`directory real path changed before cleanup: ${directory}`);
  }
  if (!pathIsWithin(ownedRoot, realPath)) {
    throw new Error(`directory escaped the probe-owned root: ${directory}`);
  }

  return { identity, realPath };
}

function verifyUnlinkIdentity(path, expectedIdentity) {
  const state = inspectPath(path);
  if (state.status === 'unknown') {
    throw state.error;
  }
  if (state.status === 'absent') {
    return false;
  }
  const identity = { dev: state.stats.dev, ino: state.stats.ino };
  if (!isSameFilesystemIdentity(identity, expectedIdentity)) {
    throw new Error(`entry identity changed before unlink: ${path}`);
  }
  return true;
}

function removeBottomUp(directory, expectedIdentity, expectedRealPath, ownedRoot) {
  verifyDirectory(
    directory,
    expectedIdentity,
    expectedRealPath,
    ownedRoot,
  );

  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const entryPath = join(directory, entry.name);
    if (!pathIsWithin(resolve(directory), resolve(entryPath), false)) {
      throw new Error(`entry path escaped its parent directory: ${entryPath}`);
    }
    const entryState = inspectPath(entryPath);
    if (entryState.status === 'unknown') {
      throw entryState.error;
    }
    if (entryState.status === 'absent') {
      continue;
    }
    const entryIdentity = {
      dev: entryState.stats.dev,
      ino: entryState.stats.ino,
    };

    if (entryState.stats.isSymbolicLink()) {
      if (verifyUnlinkIdentity(entryPath, entryIdentity)) {
        unlinkSync(entryPath);
      }
    } else if (entryState.stats.isDirectory()) {
      const entryRealPath = realpathSync.native(entryPath);
      if (!pathIsWithin(ownedRoot, entryRealPath, false)) {
        throw new Error(`child directory escaped the probe-owned root: ${entryPath}`);
      }
      removeBottomUp(entryPath, entryIdentity, entryRealPath, ownedRoot);
    } else {
      if (verifyUnlinkIdentity(entryPath, entryIdentity)) {
        unlinkSync(entryPath);
      }
    }
  }
  verifyDirectory(
    directory,
    expectedIdentity,
    expectedRealPath,
    ownedRoot,
  );
  rmdirSync(directory);
}

export function probeWindowsNodeUnicodeFilesystem({
  parentDirectory = tmpdir(),
} = {}) {
  const ownedUnicodeParent = join(parentDirectory, `测试 路径-${randomUUID()}`);
  const probeRoot = join(ownedUnicodeParent, 'probe-root');
  const testedPath = join(probeRoot, 'esbuild');
  const stagePath = join(
    probeRoot,
    `esbuild_tmp_${process.pid}_${randomUUID()}`,
  );
  const paths = { testedPath, probeRoot, ownedUnicodeParent, stagePath };
  const result = {
    nodeVersion: process.version,
    platform: process.platform,
    testedPath,
    probeRoot,
    ownedUnicodeParent,
    validatedContents: { ...STAGE_CONTENTS },
  };
  let primaryError;
  let ownership;

  try {
    const callerRealPath = realpathSync.native(parentDirectory);
    mkdirSync(ownedUnicodeParent);
    const ownedParentState = inspectPath(ownedUnicodeParent);
    if (ownedParentState.status === 'unknown') {
      throw ownedParentState.error;
    }
    if (ownedParentState.status === 'absent'
      || ownedParentState.stats.isSymbolicLink()
      || !ownedParentState.stats.isDirectory()) {
      throw new Error(`failed to establish an owned directory at ${ownedUnicodeParent}`);
    }
    const ownedRealPath = realpathSync.native(ownedUnicodeParent);
    if (!pathIsWithin(callerRealPath, ownedRealPath, false)) {
      throw new Error(`probe-owned directory escaped its caller parent: ${ownedUnicodeParent}`);
    }
    const ownedIdentity = {
      dev: ownedParentState.stats.dev,
      ino: ownedParentState.stats.ino,
    };
    verifyDirectory(
      ownedUnicodeParent,
      ownedIdentity,
      ownedRealPath,
      ownedRealPath,
    );
    ownership = {
      identity: ownedIdentity,
      realPath: ownedRealPath,
    };

    mkdirSync(probeRoot);
    writeContents(testedPath, {
      'package.json': '{"name":"probe-target"}\n',
      'bin/esbuild.exe': 'old-probe-binary\n',
      'lib/main.js': 'export const probe = false;\n',
    });
    writeContents(stagePath, STAGE_CONTENTS);

    try {
      rmSync(testedPath, { recursive: true, force: true });
    } catch (error) {
      throw createFilesystemError(
        'remove-target',
        `rmSync threw while removing ${testedPath}: ${error.message}`,
        paths,
        error,
      );
    }
    const targetAfterRemoval = inspectPath(testedPath);
    if (targetAfterRemoval.status === 'unknown') {
      throw createFilesystemError(
        'remove-target',
        `lstatSync could not determine target state after rmSync (${targetAfterRemoval.error.code}): ${targetAfterRemoval.error.message}`,
        paths,
        targetAfterRemoval.error,
      );
    }
    if (targetAfterRemoval.status === 'present') {
      throw createFilesystemError(
        'remove-target',
        `rmSync returned without throwing but ${testedPath} still exists (silent no-op)`,
        paths,
      );
    }

    try {
      renameSync(stagePath, testedPath);
    } catch (error) {
      throw createFilesystemError(
        'rename-stage',
        `renameSync failed from ${stagePath} to ${testedPath}: ${error.message}`,
        paths,
        error,
      );
    }
    const stageAfterRename = inspectPath(stagePath);
    const targetAfterRename = inspectPath(testedPath);
    const uncertainRenameState = [stageAfterRename, targetAfterRename]
      .find((state) => state.status === 'unknown');
    if (uncertainRenameState !== undefined) {
      throw createFilesystemError(
        'rename-stage',
        `lstatSync could not determine rename state (${uncertainRenameState.error.code}): ${uncertainRenameState.error.message}`,
        paths,
        uncertainRenameState.error,
      );
    }
    if (stageAfterRename.status !== 'absent'
      || targetAfterRename.status !== 'present') {
      throw createFilesystemError(
        'rename-stage',
        'renameSync returned without producing the required target state',
        paths,
      );
    }

    for (const [relativePath, expectedContents] of Object.entries(STAGE_CONTENTS)) {
      const validatedPath = join(testedPath, ...relativePath.split('/'));
      let actualContents;
      try {
        actualContents = readFileSync(validatedPath);
      } catch (error) {
        const validationError = createFilesystemError(
          'validate-content',
          `readFileSync failed for ${validatedPath}: ${error.message}`,
          paths,
          error,
        );
        validationError.failingPath = validatedPath;
        throw validationError;
      }
      if (!actualContents.equals(Buffer.from(expectedContents, 'utf8'))) {
        const validationError = createFilesystemError(
          'validate-content',
          `unexpected contents in ${validatedPath}`,
          paths,
        );
        validationError.failingPath = validatedPath;
        throw validationError;
      }
    }
  } catch (error) {
    primaryError = error.operation === undefined
      ? createFilesystemError(
        'prepare-probe',
        error.message,
        paths,
        error,
      )
      : error;
  }

  let cleanupError;
  if (ownership === undefined) {
    const unownedPathState = inspectPath(ownedUnicodeParent);
    if (unownedPathState.status === 'present') {
      cleanupError = createFilesystemError(
        'cleanup-owned-path',
        `refusing to clean ${ownedUnicodeParent} because ownership was not established`,
        paths,
      );
    } else if (unownedPathState.status === 'unknown') {
      cleanupError = createFilesystemError(
        'cleanup-owned-path',
        `lstatSync could not determine unowned path state (${unownedPathState.error.code}): ${unownedPathState.error.message}`,
        paths,
        unownedPathState.error,
      );
    }
  } else {
    try {
      verifyDirectory(
        ownedUnicodeParent,
        ownership.identity,
        ownership.realPath,
        ownership.realPath,
      );
      rmSync(ownedUnicodeParent, { recursive: true, force: true });
    } catch (error) {
      cleanupError = createFilesystemError(
        'cleanup-owned-path',
        `normal cleanup failed for ${ownedUnicodeParent}: ${error.message}`,
        paths,
        error,
      );
    }
  }
  const ownedParentAfterRemoval = inspectPath(ownedUnicodeParent);
  if (ownedParentAfterRemoval.status === 'present' && cleanupError === undefined) {
    cleanupError = createFilesystemError(
      'cleanup-owned-path',
      `rmSync returned without throwing but ${ownedUnicodeParent} still exists (silent no-op)`,
      paths,
    );
  } else if (ownedParentAfterRemoval.status === 'unknown') {
    const stateError = createFilesystemError(
      'cleanup-owned-path',
      `lstatSync could not determine cleanup state (${ownedParentAfterRemoval.error.code}): ${ownedParentAfterRemoval.error.message}`,
      paths,
      ownedParentAfterRemoval.error,
    );
    if (cleanupError === undefined) {
      cleanupError = stateError;
    } else {
      const combinedCleanupError = new AggregateError(
        [cleanupError, stateError],
        `normal cleanup and cleanup-state inspection failed for ${ownedUnicodeParent}`,
      );
      combinedCleanupError.operation = 'cleanup-owned-path';
      combinedCleanupError.testedPath = testedPath;
      combinedCleanupError.probeRoot = probeRoot;
      combinedCleanupError.ownedUnicodeParent = ownedUnicodeParent;
      combinedCleanupError.stagePath = stagePath;
      combinedCleanupError.details = `${cleanupError.details}\n${stateError.details}`;
      cleanupError = combinedCleanupError;
    }
  }

  if (cleanupError !== undefined
    && ownership !== undefined
    && ownedParentAfterRemoval.status === 'present') {
    try {
      removeBottomUp(
        ownedUnicodeParent,
        ownership.identity,
        ownership.realPath,
        ownership.realPath,
      );
    } catch (error) {
      const fallbackError = createFilesystemError(
        'cleanup-owned-path',
        `bottom-up cleanup failed for ${ownedUnicodeParent}: ${error.message}`,
        paths,
        error,
      );
      const cleanupFailure = new AggregateError(
        [cleanupError, fallbackError],
        `normal and bottom-up cleanup failed for ${ownedUnicodeParent}`,
      );
      cleanupFailure.operation = 'cleanup-owned-path';
      cleanupFailure.testedPath = testedPath;
      cleanupFailure.probeRoot = probeRoot;
      cleanupFailure.ownedUnicodeParent = ownedUnicodeParent;
      cleanupFailure.stagePath = stagePath;
      cleanupFailure.details = `${cleanupError.details}\n${fallbackError.details}`;
      cleanupError = cleanupFailure;
    }
  }

  const finalOwnedParentState = inspectPath(ownedUnicodeParent);
  const cleanupComplete = finalOwnedParentState.status === 'absent';
  if (!cleanupComplete && cleanupError === undefined) {
    const details = finalOwnedParentState.status === 'unknown'
      ? `lstatSync could not determine final cleanup state (${finalOwnedParentState.error.code}): ${finalOwnedParentState.error.message}`
      : `probe-owned path still exists after cleanup: ${ownedUnicodeParent}`;
    cleanupError = createFilesystemError(
      'cleanup-owned-path',
      details,
      paths,
      finalOwnedParentState.error,
    );
  }
  if (primaryError !== undefined) {
    primaryError.cleanupComplete = cleanupComplete;
    if (!cleanupComplete) {
      const aggregateError = new AggregateError(
        [primaryError, cleanupError],
        `filesystem probe failed and cleanup was incomplete for ${ownedUnicodeParent}`,
      );
      aggregateError.operation = primaryError.operation;
      aggregateError.testedPath = testedPath;
      aggregateError.probeRoot = probeRoot;
      aggregateError.ownedUnicodeParent = ownedUnicodeParent;
      aggregateError.stagePath = stagePath;
      aggregateError.cleanupComplete = false;
      throw aggregateError;
    }
    throw primaryError;
  }
  if (!cleanupComplete) {
    cleanupError.cleanupComplete = cleanupComplete;
    throw cleanupError;
  }

  return result;
}

function appendErrorDetails(lines, error, label, seen) {
  if (error === null || typeof error !== 'object') {
    lines.push(`${label}: ${String(error)}`);
    return;
  }
  if (seen.has(error)) {
    lines.push(`${label}: [already reported]`);
    return;
  }
  seen.add(error);

  const name = error.name ?? error.constructor?.name ?? 'Error';
  lines.push(`${label}: ${name}: ${error.message ?? String(error)}`);
  if (error.details !== undefined) {
    lines.push(`${label} details: ${error.details}`);
  }
  if (error.cause !== undefined) {
    appendErrorDetails(lines, error.cause, `${label} cause`, seen);
  }
  if (error instanceof AggregateError) {
    for (const [index, nestedError] of [...error.errors].entries()) {
      appendErrorDetails(
        lines,
        nestedError,
        `${label} AggregateError[${index}]`,
        seen,
      );
    }
  }
  if (error.cleanupError !== undefined) {
    appendErrorDetails(lines, error.cleanupError, `${label} cleanup`, seen);
  }
}

export function formatWindowsNodeUnicodeFilesystemError(
  error,
  version = process.version,
  platform = process.platform,
) {
  const lines = [
    'Windows Node Unicode filesystem capability probe failed.',
    `Node version: ${version}`,
    `platform: ${platform}`,
    `operation: ${error?.operation ?? 'unknown'}`,
    `tested path: ${error?.testedPath ?? 'unknown'}`,
    `probe root: ${error?.probeRoot ?? 'unknown'}`,
    `owned Unicode parent: ${error?.ownedUnicodeParent ?? 'unknown'}`,
    `cleanupComplete: ${error?.cleanupComplete ?? 'unknown'}`,
    `Supported Node ranges: ${SUPPORTED_NODE_RANGES.join(', ')}`,
    `Known Node issue: ${NODE_ISSUE_URL}`,
  ];
  appendErrorDetails(lines, error, 'Failure', new Set());
  return lines.join('\n');
}

const isDirectExecution = process.argv[1] !== undefined
  && pathToFileURL(process.argv[1]).href === import.meta.url;

if (isDirectExecution) {
  let versionAccepted = true;
  try {
    assertSupportedWindowsNodeVersion();
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
    versionAccepted = false;
  }

  if (versionAccepted) {
    try {
      const result = probeWindowsNodeUnicodeFilesystem();
      console.log(
        `Windows Node Unicode filesystem probe passed: ${result.nodeVersion} ${result.platform}`,
      );
    } catch (error) {
      console.error(formatWindowsNodeUnicodeFilesystemError(error));
      process.exitCode = 1;
    }
  }
}
