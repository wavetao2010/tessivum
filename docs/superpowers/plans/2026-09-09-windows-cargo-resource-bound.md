# Windows Cargo resource bound implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Require one Cargo build job for every Windows CI and native acceptance
command that compiles or links code.

**Architecture:** Extend the existing independent Node and Python structural
parsers. Each parser enumerates executable Cargo command lines only inside
`jobs.windows` or approved PowerShell Markdown fences, requires exact bounded
commands, and runs mutation fixtures that prove decoys cannot compensate for a
missing or malformed command.

**Tech stack:** GitHub Actions YAML source contracts, Node.js test runner,
Python 3, PowerShell Markdown procedures, Cargo.

---

## File map

The implementation changes two contracts and their two protected sources.

- Modify `scripts/check-windows-node-unicode-fs.test.mjs` for the Node contract.
- Modify `scripts/check_compat_baseline.py` for the Python contract.
- Modify `.github/workflows/ci.yml` only inside `jobs.windows`.
- Modify `docs/WINDOWS_SOURCE_TEST.md` only for resource prerequisites and
  compiling/linking Cargo command lines.

### Task 1: Bound all Windows CI Cargo commands

This task establishes Node and Python RED against the live unbounded workflow,
then changes only `jobs.windows`.

**Files:**

- Modify: `scripts/check-windows-node-unicode-fs.test.mjs`
- Modify: `scripts/check_compat_baseline.py`
- Modify: `.github/workflows/ci.yml:195`

- [ ] **Step 1: Define the exact Windows CI command inventory**

Add the following six-command expected inventory to both contracts:

```text
cargo check --jobs 1 --all-targets --locked
cargo clippy --jobs 1 --all-targets --locked -- -D warnings
cargo test --jobs 1 --all-targets --locked --no-fail-fast
cargo build --jobs 1 --locked --release --target wasm32-unknown-unknown
cargo test --jobs 1 --locked --test wasm_plugins real_guest_denies_undeclared_service_and_traps_deterministically -- --exact
cargo test --jobs 1 --locked --test community_plugins vendored_timer_loads_unchanged_through_the_legacy_profile_and_reaps_after_disconnect -- --exact
```

Use `parseWindowsWorkflow` and `parse_windows_workflow` output. Enumerate only
trimmed `run` lines whose executable token is `cargo`; do not scan the raw YAML.
Require `cargo fetch --locked` separately and exclude it from the bounded list.

- [ ] **Step 2: Add Node and Python live-source assertions**

In Node, add an assertion helper and one test that reads `ci.yml`, isolates
`jobs.windows`, and compares the actual compile/link command inventory to the
six exact strings above.

In Python, add the equivalent `check_windows_ci_cargo_bounds` function, call it
from `main`, and append stable failure messages to the shared `failures` list.

- [ ] **Step 3: Run both contracts and verify RED**

```powershell
node --test scripts/check-windows-node-unicode-fs.test.mjs
python scripts/check_compat_baseline.py
```

Expected: both commands fail specifically because the live Windows CI Cargo
commands lack `--jobs 1`. Fix test or parser errors until both are behavior RED.

- [ ] **Step 4: Add the mandatory workflow mutation matrix**

For both implementations, mutate one uniquely anchored bounded command at a
time and require the exact expected contract error for:

- removing `--jobs 1`;
- replacing it with `--jobs 2`;
- replacing it with `-j 1`;
- replacing it with `--jobs=1`;
- moving the exact command to another workflow job while leaving the Windows
  command unbounded;
- leaving the exact command only in a YAML comment while the executable command
  is unbounded.

Use the existing unique-anchor replacement helpers. Each fixture must call the
Cargo contract directly and cannot pass because an unrelated prerequisite
contract failed first.

- [ ] **Step 5: Update only the Windows workflow commands**

Change the six `jobs.windows` commands to exactly match the inventory in Step
1. Keep `cargo fetch --locked`, all non-Windows jobs, test selection,
   `--locked`, `--no-fail-fast`, working directories, and later `--`
   separators unchanged.

- [ ] **Step 6: Verify Task 1 is GREEN on both Node runtimes and Python**

```powershell
node --test scripts/check-windows-node-unicode-fs.test.mjs
& 'C:\Users\Q\Documents\New project\.tools\node-v24.20.0-win-x64\node.exe' `
  --test scripts/check-windows-node-unicode-fs.test.mjs
python scripts/check_compat_baseline.py
```

Expected: Node 24.11.1 retains only its documented expected skips, Node 24.20.0
retains its documented expected skip, Python exits 0, and no mutation passes.

- [ ] **Step 7: Inspect and commit Task 1**

```powershell
git diff --check
git diff -- .github/workflows/ci.yml `
  scripts/check-windows-node-unicode-fs.test.mjs `
  scripts/check_compat_baseline.py
git add -- .github/workflows/ci.yml `
  scripts/check-windows-node-unicode-fs.test.mjs `
  scripts/check_compat_baseline.py
git commit -m "ci: bound Windows Cargo build jobs"
```

Run Task 1 specification review and then code-quality review. Resolve every
Critical or Important finding and repeat the relevant review before Task 2.

### Task 2: Bound native Windows source acceptance commands

This task extends both contracts to executable PowerShell Markdown blocks before
changing the source procedure.

**Files:**

- Modify: `scripts/check-windows-node-unicode-fs.test.mjs`
- Modify: `scripts/check_compat_baseline.py`
- Modify: `docs/WINDOWS_SOURCE_TEST.md:245`

- [ ] **Step 1: Define the exact source-guide command inventory**

Require these six logical commands, joining PowerShell backtick continuations
before comparison:

```text
cargo check --jobs 1 --all-targets --locked
cargo clippy --jobs 1 --all-targets --locked -- -D warnings
cargo test --jobs 1 --all-targets --locked
& cargo run --jobs 1 --locked -- --session windows-smoke --data-dir $state --replay fixtures/headless/recorded-replay.jsonl --trusted-bash 'prove the CLI tool round trip'
& cargo run --jobs 1 --locked -- --session windows-smoke --data-dir $state --replay fixtures/headless/recorded-replay.jsonl --trusted-bash --resume 'prove the CLI tool round trip'
cargo run --jobs 1 --release -- web
```

Groups 10-15 must come from the existing main ```` ```powershell ```` block
selected by its `#Requires -Version 7.4` anchor. Group 16 must come from the
executable `powershell` block introduced by the terminal A instruction and
containing `Start-Transcript`. A plain `text` fence is never executable
evidence.

- [ ] **Step 2: Add Node and Python live-guide assertions**

Reuse Node's `sectionBetween` for the main block and add a narrowly anchored
executable-fence extractor for Group 16. Add equivalent Python helpers that
require exactly one matching main block and one matching terminal A block.

Normalize CRLF and PowerShell backtick continuation only. Do not remove comments
or search outside the selected blocks. Compare the resulting Cargo inventory to
the six exact logical commands from Step 1.

- [ ] **Step 3: Run both contracts and verify source-guide RED**

```powershell
node --test scripts/check-windows-node-unicode-fs.test.mjs
python scripts/check_compat_baseline.py
```

Expected: both fail specifically on the first unbounded executable source-guide
Cargo command. The already-bounded CI contract from Task 1 remains GREEN.

- [ ] **Step 4: Add the mandatory Markdown mutation matrix**

Both Node and Python contracts must directly reject, with exact failure text:

- Group 12 with `--jobs 1` removed;
- a source command changed to `--jobs 2`, `-j 1`, or `--jobs=1`;
- an unbounded executable command plus the exact bounded command only in a
  PowerShell comment;
- an unbounded executable command plus the exact bounded command only in a
  separate ````text` fence;
- a duplicate or missing main/terminal A executable block.

The bounded comment or `text` fence must not compensate for the malformed
executable command.

- [ ] **Step 5: Update the source procedure and resource prerequisite**

Add `--jobs 1` to Groups 10, 11, 12, 14, 15, and 16 exactly as shown in Step 1.
Add a concise prerequisite stating that the selected NTFS work volume needs
enough free capacity for dependency trees and normal Rust test artifacts.

Do not add a drive letter, numeric free-space threshold, retry, cleanup command,
test exclusion, or reduced debug profile.

- [ ] **Step 6: Verify Task 2 is GREEN**

```powershell
node --test scripts/check-windows-node-unicode-fs.test.mjs
& 'C:\Users\Q\Documents\New project\.tools\node-v24.20.0-win-x64\node.exe' `
  --test scripts/check-windows-node-unicode-fs.test.mjs
python scripts/check_compat_baseline.py
$guide = Get-Content -Raw -LiteralPath 'docs/WINDOWS_SOURCE_TEST.md'
$blocks = [regex]::Matches($guide, '(?ms)^```powershell\r?\n(.*?)^```')
foreach ($block in $blocks) {
  $tokens = $null
  $errors = $null
  [Management.Automation.Language.Parser]::ParseInput(
    $block.Groups[1].Value,
    [ref]$tokens,
    [ref]$errors
  ) | Out-Null
  if ($errors.Count) { $errors; exit 1 }
}
```

Expected: both Node matrices and Python exit 0. The documented PowerShell
commands remain syntactically valid when extracted according to the existing
procedure validation method.

- [ ] **Step 7: Inspect and commit Task 2**

```powershell
git diff --check
git diff -- docs/WINDOWS_SOURCE_TEST.md `
  scripts/check-windows-node-unicode-fs.test.mjs `
  scripts/check_compat_baseline.py
git add -- docs/WINDOWS_SOURCE_TEST.md `
  scripts/check-windows-node-unicode-fs.test.mjs `
  scripts/check_compat_baseline.py
git commit -m "test: bound Windows source Cargo jobs"
```

Run Task 2 specification review and then code-quality review. Resolve and
re-review all Critical or Important findings before acceptance.

### Task 3: Verify the complete resource contract

This task verifies the protected sources and prepares a fresh acceptance run.

**Files:**

- Verify: `.github/workflows/ci.yml`
- Verify: `docs/WINDOWS_SOURCE_TEST.md`
- Verify: both source-contract scripts

- [ ] **Step 1: Run all repository source contracts**

```powershell
node --test scripts/check-windows-node-unicode-fs.test.mjs
& 'C:\Users\Q\Documents\New project\.tools\node-v24.20.0-win-x64\node.exe' `
  --test scripts/check-windows-node-unicode-fs.test.mjs
python scripts/check_compat_baseline.py
python scripts/check_release_facts.py
git diff --check
```

Expected: every command exits 0 with only documented Node skips.

- [ ] **Step 2: Audit command scope**

Confirm `cargo fetch --locked` is unchanged, every `jobs.windows` compile/link
command has exact `--jobs 1`, all non-Windows workflow jobs are byte-for-byte
unchanged from the Task 1 base, and only Groups 10-12, 14-15, 16 plus resource
prerequisite text changed in the source guide.

- [ ] **Step 3: Review the complete Cargo resource implementation**

Dispatch a final read-only reviewer across the Task 1 base SHA through the Task
2 head SHA. Resolve all Critical and Important findings, rerun the complete
source-contract commands, and immediately reclaim the reviewer.

- [ ] **Step 4: Reserve a fresh acceptance root**

Select a previously unused Chinese-and-space path on D: or E: with sufficient
NTFS capacity. Do not create, clean, reuse, or rerun either preserved failed
acceptance checkout. The full Groups 01-16 procedure begins only after both
implementation plans and their final reviews are complete.
