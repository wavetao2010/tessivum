import { randomUUID } from 'node:crypto';
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  rmdirSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
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

function removeBottomUp(directory) {
  if (!existsSync(directory)) {
    return;
  }

  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const entryPath = join(directory, entry.name);
    if (entry.isDirectory()) {
      removeBottomUp(entryPath);
    } else {
      unlinkSync(entryPath);
    }
  }
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

  try {
    mkdirSync(probeRoot, { recursive: true });
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
    if (existsSync(testedPath)) {
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
    if (existsSync(stagePath) || !existsSync(testedPath)) {
      throw createFilesystemError(
        'rename-stage',
        'renameSync returned without producing the required target state',
        paths,
      );
    }

    for (const [relativePath, expectedContents] of Object.entries(STAGE_CONTENTS)) {
      const validatedPath = join(testedPath, ...relativePath.split('/'));
      const actualContents = readFileSync(validatedPath);
      if (!actualContents.equals(Buffer.from(expectedContents, 'utf8'))) {
        throw createFilesystemError(
          'validate-content',
          `unexpected contents in ${validatedPath}`,
          paths,
        );
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
  try {
    rmSync(ownedUnicodeParent, { recursive: true, force: true });
  } catch (error) {
    cleanupError = createFilesystemError(
      'cleanup-owned-path',
      `rmSync threw while cleaning ${ownedUnicodeParent}: ${error.message}`,
      paths,
      error,
    );
  }
  if (existsSync(ownedUnicodeParent) && cleanupError === undefined) {
    cleanupError = createFilesystemError(
      'cleanup-owned-path',
      `rmSync returned without throwing but ${ownedUnicodeParent} still exists (silent no-op)`,
      paths,
    );
  }

  if (cleanupError !== undefined) {
    try {
      removeBottomUp(ownedUnicodeParent);
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
      cleanupError = cleanupFailure;
    }
  }

  const cleanupComplete = !existsSync(ownedUnicodeParent);
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
