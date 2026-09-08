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
const pwshResolution = spawnSync('pwsh', [
  '-NoProfile',
  '-Command',
  '(Get-Command pwsh -ErrorAction Stop).Source',
], { encoding: 'utf8' });
const pwshPath = pwshResolution.status === 0
  ? pwshResolution.stdout.trim()
  : null;
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

function replaceWorkflowOnce(workflow, current, replacement) {
  const firstIndex = workflow.indexOf(current);
  const lastIndex = workflow.lastIndexOf(current);
  assert.notEqual(firstIndex, -1, `workflow fixture is missing: ${current}`);
  assert.equal(
    firstIndex,
    lastIndex,
    `workflow fixture must contain exactly one anchor: ${current}`,
  );
  return workflow.replace(current, replacement);
}

function stripYamlComment(value) {
  let quote = null;
  for (let index = 0; index < value.length; index += 1) {
    const character = value[index];
    if (!quote && (character === "'" || character === '"')) {
      quote = character;
    } else if (character === quote && value[index - 1] !== '\\') {
      quote = null;
    } else if (!quote && character === '#' && /(^|\s)/.test(value[index - 1] ?? '')) {
      return value.slice(0, index).trimEnd();
    }
  }
  return value.trimEnd();
}

function workflowJobKey(line) {
  const match = /^  (['"]?)([A-Za-z0-9_-]+)\1:\s*$/.exec(line);
  return match?.[2] ?? null;
}

function parseWindowsWorkflow(workflow) {
  const lines = workflow.replaceAll('\r\n', '\n').replaceAll('\r', '\n')
    .split('\n');
  const visible = lines.map(stripYamlComment);
  const jobs = visible
    .map((line, index) => line === 'jobs:' ? index : -1)
    .filter((index) => index >= 0);
  assert.equal(jobs.length, 1, 'Windows CI workflow must define jobs once');

  const windowsJobs = [];
  for (let index = jobs[0] + 1; index < visible.length; index += 1) {
    if (visible[index].trim() && !visible[index].startsWith(' ')) break;
    if (workflowJobKey(visible[index]) === 'windows') windowsJobs.push(index);
  }
  assert.equal(
    windowsJobs.length,
    1,
    'Windows CI workflow must define jobs.windows exactly once',
  );

  const jobStart = windowsJobs[0];
  let jobEnd = lines.length;
  for (let index = jobStart + 1; index < visible.length; index += 1) {
    if (workflowJobKey(visible[index]) !== null) {
      jobEnd = index;
      break;
    }
  }
  const jobLines = lines.slice(jobStart + 1, jobEnd);
  const runsOnValues = jobLines
    .map((line) => /^    runs-on:\s*(.*)$/.exec(stripYamlComment(line)))
    .filter(Boolean)
    .map((match) => match[1].replace(/^(['"])(.*)\1$/, '$2'));
  assert.equal(runsOnValues.length, 1, 'jobs.windows must contain runs-on');
  const stepsIndex = jobLines.findIndex(
    (line) => stripYamlComment(line).trimEnd() === '    steps:',
  );
  assert.notEqual(stepsIndex, -1, 'jobs.windows must contain steps');

  const stepLines = [];
  for (const line of jobLines.slice(stepsIndex + 1)) {
    const structural = stripYamlComment(line);
    const indent = structural.length - structural.trimStart().length;
    if (structural.trim() && indent <= 4) break;
    stepLines.push(line);
  }
  const itemStarts = [];
  for (let index = 0; index < stepLines.length; index += 1) {
    if (/^ {6}-(?:\s|$)/.test(stripYamlComment(stepLines[index]))) {
      itemStarts.push(index);
    }
  }
  const steps = itemStarts.map((start, itemIndex) => {
    const end = itemStarts[itemIndex + 1] ?? stepLines.length;
    const item = stepLines.slice(start, end);
    item[0] = `        ${item[0].slice(8)}`;
    const step = { with: {}, run: [], unconsumed: [] };
    let mode = null;
    for (const rawLine of item) {
      const indent = /^ */.exec(rawLine)[0].length;
      if (mode === 'run' && indent >= 10) {
        const command = rawLine.slice(10).trim();
        if (command) step.run.push(command);
        continue;
      }
      const line = stripYamlComment(rawLine).trim();
      if (!line) continue;
      if (mode === 'with' && indent === 10) {
        const match = /^(repository|ref|node-version|bun-version|version):\s*(.*)$/.exec(line);
        if (match) {
          step.with[match[1]] = match[2].replace(/^(['"])(.*)\1$/, '$2');
        } else {
          step.unconsumed.push(rawLine);
        }
        continue;
      }
      if (indent !== 8) {
        step.unconsumed.push(rawLine);
        continue;
      }
      mode = null;
      const match = /^(['"]?)(uses|name|with|run|if|continue-on-error)\1:\s*(.*)$/.exec(line);
      if (!match) {
        step.unconsumed.push(rawLine);
        continue;
      }
      const [, , key, rawValue] = match;
      const value = rawValue.replace(/^(['"])(.*)\1$/, '$2');
      if (key === 'with') mode = 'with';
      else if (key === 'run') {
        if (value === '|') mode = 'run';
        else if (!/^[|>]/.test(value)) step.run.push(value);
      } else step[key] = value;
    }
    return step;
  });
  return { runsOn: runsOnValues[0], steps };
}

function parseWindowsWorkflowSteps(workflow) {
  return parseWindowsWorkflow(workflow).steps;
}

function assertWindowsCiPrerequisiteOrder(workflow) {
  const { runsOn, steps } = parseWindowsWorkflow(workflow);
  assert.equal(runsOn, 'windows-2025', 'Windows CI runner must be windows-2025');
  const checkout = 'actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09';
  assert.equal(
    steps[0]?.uses,
    checkout,
    'Windows CI first step must be the primary checkout',
  );
  assert.equal(
    steps[0]?.with.repository,
    undefined,
    'Windows CI first step must not override the primary repository',
  );
  const setupNode = steps[1];
  const nodePrerequisite = steps[2];
  assert.ok(setupNode, 'Windows CI second step must be the pinned setup-node action');
  assert.ok(nodePrerequisite, 'Windows CI Node prerequisite must be present');
  const prerequisiteCommands = [
    "$ErrorActionPreference = 'Stop'",
    '$PSNativeCommandUseErrorActionPreference = $true',
    'node --test scripts/check-windows-node-unicode-fs.test.mjs',
    'node scripts/check-windows-node-unicode-fs.mjs',
  ];
  for (const [label, step] of [
    ['setup-node', setupNode],
    ['Node prerequisite', nodePrerequisite],
  ]) {
    assert.equal(step?.if, undefined, `Windows CI ${label} must not have if`);
    assert.equal(
      step?.['continue-on-error'],
      undefined,
      `Windows CI ${label} must not continue on error`,
    );
  }
  for (const [label, step] of [
    ['setup-node', setupNode],
    ['Node prerequisite', nodePrerequisite],
  ]) {
    if (!Array.isArray(step.unconsumed) || step.unconsumed.length !== 0) {
      const error = new Error(`Windows CI ${label} contains unconsumed syntax`);
      error.name = '';
      throw error;
    }
  }
  assert.deepEqual(setupNode, {
    uses: 'actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38',
    with: { 'node-version': '24.20.0' },
    run: [],
    unconsumed: [],
  }, 'Windows CI setup-node changed or moved');
  assert.deepEqual(nodePrerequisite, {
    name: 'Verify Windows Node filesystem prerequisite',
    with: {},
    run: prerequisiteCommands,
    unconsumed: [],
  }, 'Windows CI Node prerequisite changed or moved');

  const externalRepositories = [
    ['deepseek-ai/deepseek-harness', '47f943859bef60e4160492346772ded9b24f765a'],
    ['cordiverse/cordis', '8cc9e33fab69e2d0476d126baaf2acb24e6a6ab4'],
    ['wavetao2010/tessivum-core', '86c7e1c71bd99a3c0fc70e7be6f251c89f2cc694'],
  ];
  for (const [offset, [repository, ref]] of externalRepositories.entries()) {
    const step = steps[offset + 3];
    assert.equal(step?.uses, checkout, `Windows CI external checkout ${offset + 1}`);
    assert.equal(step?.with.repository, repository);
    assert.equal(step?.with.ref, ref);
  }

  const laterRequirements = [
    (step) => step.uses
      === 'dtolnay/rust-toolchain@032958afbdc797a9164d3bc0b56325c1308924a5',
    (step) => step.uses
      === 'oven-sh/setup-bun@0c5077e51419868618aeaa5fe8019c62421857d6'
      && step.with['bun-version'] === '1.4.0',
    (step) => step.uses
      === 'pnpm/action-setup@b906affcce14559ad1aafd4ab0e942779e9f58b1'
      && step.with.version === '11.7.0',
    (step) => step.name === 'Install pinned DeepSeek build dependencies'
      && step.run.length === 1
      && step.run[0] === 'pnpm install --frozen-lockfile',
  ];
  let cursor = 6;
  for (const requirement of laterRequirements) {
    const index = steps.findIndex((step, stepIndex) => (
      stepIndex >= cursor && requirement(step)
    ));
    assert.notEqual(index, -1, 'Windows CI later prerequisite step is missing');
    cursor = index + 1;
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
  assertWindowsCiPrerequisiteOrder(workflow);
});

const windowsCiSemanticVariants = [
  [
    'a non-Windows runner',
    '    runs-on: windows-2025',
    '    runs-on: ubuntu-latest',
    /Windows CI runner must be windows-2025/,
  ],
  [
    'setup-node with if',
    '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38',
    '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        if: false',
    /Windows CI setup-node must not have if/,
  ],
  [
    'setup-node with continue-on-error',
    '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38',
    '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        continue-on-error: true',
    /Windows CI setup-node must not continue on error/,
  ],
  [
    'the Node prerequisite with if',
    '      - name: Verify Windows Node filesystem prerequisite',
    '      - name: Verify Windows Node filesystem prerequisite\n        if: false',
    /Windows CI Node prerequisite must not have if/,
  ],
  [
    'the Node prerequisite with continue-on-error',
    '      - name: Verify Windows Node filesystem prerequisite',
    '      - name: Verify Windows Node filesystem prerequisite\n        continue-on-error: true',
    /Windows CI Node prerequisite must not continue on error/,
  ],
  [
    'setup-node with an explicit mapping if key',
    '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38',
    '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        ? if\n        : false',
    /^Windows CI setup-node contains unconsumed syntax$/,
  ],
  [
    'the Node prerequisite with an explicit mapping continue-on-error key',
    '      - name: Verify Windows Node filesystem prerequisite',
    '      - name: Verify Windows Node filesystem prerequisite\n        ? continue-on-error\n        : true',
    /^Windows CI Node prerequisite contains unconsumed syntax$/,
  ],
  [
    'setup-node with shell',
    '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38',
    '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38\n        shell: pwsh',
    /^Windows CI setup-node contains unconsumed syntax$/,
  ],
  [
    'the Node prerequisite with env and NODE_OPTIONS',
    '      - name: Verify Windows Node filesystem prerequisite',
    '      - name: Verify Windows Node filesystem prerequisite\n        env:\n          NODE_OPTIONS: --no-addons',
    /^Windows CI Node prerequisite contains unconsumed syntax$/,
  ],
  [
    'the Node prerequisite with working-directory',
    '      - name: Verify Windows Node filesystem prerequisite',
    '      - name: Verify Windows Node filesystem prerequisite\n        working-directory: scripts',
    /^Windows CI Node prerequisite contains unconsumed syntax$/,
  ],
  [
    'setup-node with an unknown with input',
    '          node-version: 24.20.0',
    '          node-version: 24.20.0\n          cache: npm',
    /^Windows CI setup-node contains unconsumed syntax$/,
  ],
  ...[
    [
      'setup-node',
      '      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38',
      /Windows CI setup-node must not have if/,
      /Windows CI setup-node must not continue on error/,
    ],
    [
      'the Node prerequisite',
      '      - name: Verify Windows Node filesystem prerequisite',
      /Windows CI Node prerequisite must not have if/,
      /Windows CI Node prerequisite must not continue on error/,
    ],
  ].flatMap(([step, anchor, ifError, continueOnError]) => [
    [`${step} with single-quoted if`, anchor, `${anchor}\n        'if': false`, ifError],
    [`${step} with double-quoted if`, anchor, `${anchor}\n        "if": false`, ifError],
    [
      `${step} with single-quoted continue-on-error`,
      anchor,
      `${anchor}\n        'continue-on-error': true`,
      continueOnError,
    ],
    [
      `${step} with double-quoted continue-on-error`,
      anchor,
      `${anchor}\n        "continue-on-error": true`,
      continueOnError,
    ],
  ]),
];

for (const [
  label,
  current,
  replacement,
  expectedError,
] of windowsCiSemanticVariants) {
  test(`Windows CI rejects ${label}`, () => {
    const workflow = readFileSync(
      join(repoRoot, '.github', 'workflows', 'ci.yml'),
      'utf8',
    );
    const variant = replaceWorkflowOnce(workflow, current, replacement);
    assert.throws(
      () => assertWindowsCiPrerequisiteOrder(variant),
      expectedError,
    );
  });
}

test('workflow mutation rejects an ambiguous prerequisite anchor', () => {
  const anchor = '      - name: Verify Windows Node filesystem prerequisite';
  const workflow = `      # decoy: ${anchor}\n${anchor}`;
  assert.throws(
    () => replaceWorkflowOnce(workflow, anchor, `${anchor}\n        if: false`),
    /workflow fixture must contain exactly one anchor/,
  );
});

test('Windows CI parser rejects comments and unrelated block scalars', () => {
  const workflow = `
jobs:
  windows:
    runs-on: windows-2025
    # uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09
    steps:
      - name: "actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38"
        notes: |
          uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09
  browser-e2e:
    steps: []
`;

  assert.throws(
    () => assertWindowsCiPrerequisiteOrder(workflow),
    /Windows CI first step/,
  );
});

test('Windows CI parser stops at an inserted same-indent job', () => {
  const workflow = `
jobs:
  windows:
    runs-on: windows-2025
    steps:
      - uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09
  inserted-job:
    steps:
      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38
  browser-e2e:
    steps: []
`;

  assert.throws(
    () => assertWindowsCiPrerequisiteOrder(workflow),
    /Windows CI second step/,
  );
});

test('Windows CI parser stops at a quoted following job', () => {
  const workflow = `
jobs:
  windows:
    runs-on: windows-2025
  'quoted-job':
    steps:
      - uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09
`;

  assert.throws(
    () => parseWindowsWorkflowSteps(workflow),
    /jobs\.windows must contain steps/,
  );
});

test('Windows CI parser rejects folded run scalars', () => {
  const steps = parseWindowsWorkflowSteps(`
jobs:
  windows:
    runs-on: windows-2025
    steps:
      - run: >
          node scripts/check-windows-node-unicode-fs.mjs
  next-job:
    steps: []
`);

  assert.deepEqual(steps[0].run, []);
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
  const developerModeRead = sectionBetween(
    mainScript,
    '  $developerModePath =',
    '  $developerModeProperty = $null',
    'Developer Mode registry read',
  );
  assertTokensInOrder(developerModeRead, [
    'try {',
    'Get-ItemProperty -Path $developerModePath -ErrorAction Stop',
    '} catch [System.Management.Automation.ItemNotFoundException] {',
    '$developerModeKey = $null',
  ], 'Developer Mode registry read');
  assert.ok(
    !developerModeRead.includes('SilentlyContinue'),
    'Developer Mode registry read must not suppress unexpected errors',
  );
  const earlyExecutableEvidence = sectionBetween(
    mainScript,
    '  $earlyCommands |',
    '  @(\n    & $Git.Source --version',
    'early executable evidence',
  );
  assertTokensInOrder(earlyExecutableEvidence, [
    'ConvertTo-Json -Compress',
    "Set-Content (Join-Path $Evidence 'executables.jsonl')",
  ], 'early executable evidence');
  assert.doesNotMatch(earlyExecutableEvidence, /Format-Table|Out-String/);

  const pnpmExecutableEvidence = sectionBetween(
    mainScript,
    '  $Pnpm |',
    '  & $Pnpm.Source --version',
    'pnpm executable evidence',
  );
  assertTokensInOrder(pnpmExecutableEvidence, [
    'ConvertTo-Json -Compress',
    "Add-Content (Join-Path $Evidence 'executables.jsonl')",
  ], 'pnpm executable evidence');
  assert.doesNotMatch(pnpmExecutableEvidence, /Format-Table|Out-String/);
  assertTokensInOrder(mainScript, [
    '$Git = Get-Command git -ErrorAction Stop',
    '$Node = Get-Command node -ErrorAction Stop',
    '$Rustup = Get-Command rustup -ErrorAction Stop',
    '$Rustc = Get-Command rustc -ErrorAction Stop',
    '$Cargo = Get-Command cargo -ErrorAction Stop',
    '$Bun = Get-Command bun -ErrorAction Stop',
    '$Python = Get-Command python -ErrorAction Stop',
    '$Pwsh = Get-Command pwsh -ErrorAction Stop',
    '& $Git.Source --version',
    '& $Node.Source --version',
    '& $Rustup.Source --version',
    '& $Rustc.Source --version',
    '& $Cargo.Source --version',
    '& $Bun.Source --version',
    '& $Python.Source --version',
    '& $Pwsh.Source --version',
    'git clone https://github.com/wavetao2010/tessivum.git $Repo',
    '& $Node.Source scripts/check-windows-node-unicode-fs.mjs',
    'git clone https://github.com/deepseek-ai/deepseek-harness.git',
    'git clone https://github.com/cordiverse/cordis.git',
    'git clone https://github.com/wavetao2010/tessivum-core.git',
    '$Pnpm = Get-Command pnpm -ErrorAction Stop',
    '& $Pnpm.Source --version',
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

test('PowerShell executable evidence preserves a long Source exactly', {
  skip: pwshPath ? false : 'pwsh is unavailable',
}, () => {
  const parentDirectory = createTestOwnedParent();
  const evidencePath = join(parentDirectory, 'executables.jsonl');
  const source = `C:\\Program Files\\${'x'.repeat(300)}\\tool.exe`;
  const expected = { Name: 'synthetic-tool', Source: source, Version: '1.2.3.4' };
  assert.ok(source.length >= 269);

  const result = spawnSync(pwshPath, ['-NoProfile', '-Command', String.raw`
    $command = [pscustomobject]@{
      Name = $env:TEST_NAME
      Source = $env:TEST_SOURCE
      Version = [version] $env:TEST_VERSION
    }
    $command | ForEach-Object {
      [pscustomobject]@{
        Name = $_.Name
        Source = $_.Source
        Version = [string] $_.Version
      } | ConvertTo-Json -Compress
    } | Set-Content -LiteralPath $env:TEST_EVIDENCE_PATH
  `], {
    encoding: 'utf8',
    env: {
      ...process.env,
      TEST_EVIDENCE_PATH: evidencePath,
      TEST_NAME: expected.Name,
      TEST_SOURCE: expected.Source,
      TEST_VERSION: expected.Version,
    },
  });

  assert.equal(result.status, 0, result.stderr);
  const records = readFileSync(evidencePath, 'utf8')
    .trim()
    .split(/\r?\n/)
    .map((line) => JSON.parse(line));
  assert.deepEqual(records, [expected]);
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
