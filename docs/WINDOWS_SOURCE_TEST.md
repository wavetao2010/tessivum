# Windows 11 ordinary-user source acceptance

This procedure validates source on native Windows 11 x64. It is not evidence for a ZIP,
`install.ps1`, upgrade, uninstall, package build, or package publication. Windows Server CI
does not replace this ordinary-user run.

The retained 2026-09-07 run at `630fe949cb8908665ca6d0bad5ba6410a6bc4446`
failed with pnpm `esbuild_tmp_* -> esbuild` rename `EPERM`, then two Market file-symlink
fixtures failed with `EPERM`. Fail-fast execution prevents those failures from being masked;
it does not establish that either failure is fixed. This document contains no new Windows
evidence.

The original raw logs are unavailable; recovering them is not a prerequisite for this run.
Test the exact new candidate commit and retain fresh evidence from the start. If installation
fails, stop and preserve the transcript and checkout before cleanup or retry. If all acceptance
requirements pass, report that this commit passed in this environment; record the historical
`EPERM` as not reproduced, not as a proven root-cause fix.

## Environment

Use Windows 11 x64, NTFS, an ordinary account, Developer Mode off, and a checkout path with
Chinese characters and a space. Do not use WSL, elevation, Developer Mode, disabled security
software, global store/proxy changes, hand-edited `node_modules`, blind retries, or tool
version changes as workarounds.

Install Visual Studio 2022 Build Tools (**Desktop development with C++**), Git, PowerShell 7.4+,
Node.js, Rust stable, and Python 3. Bun `1.4.0` and pnpm `11.7.0` remain fixed. The failed run
used Windows 11 25H2 build 26200, PowerShell 7.6.4, Git 2.51.2.windows.1, Node 24.11.1,
Rust/Cargo 1.94.0, Bun 1.4.0, pnpm 11.7.0, and Python 3.12.10. Record every actual version,
resolved executable path, and difference; do not force or downgrade unrelated tools merely
to reproduce those patch versions.

## Command groups 1–15

Save the block as `windows-source-test.ps1` outside the checkout and run it from PowerShell 7.4+:

```powershell
pwsh -NoProfile -File .\windows-source-test.ps1 -TessivumRevision <current-40-character-candidate-commit>
```

Supply the current candidate commit, not the old report commit. The single `try` scope and
PowerShell native-command preference make any non-zero native exit terminate the script.
Later groups are then `NOT RUN`, and the overall result is `FAIL`; an unrun required group is
never a pass.

```powershell
#Requires -Version 7.4
param(
  [Parameter(Mandatory)]
  [ValidatePattern('^[0-9a-fA-F]{40}$')]
  [string] $TessivumRevision
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$WorkRoot = Join-Path $HOME '测试 项目'
$Repo = Join-Path $WorkRoot ("tessivum-" + $TessivumRevision.Substring(0, 12).ToLowerInvariant())
$Evidence = Join-Path $HOME ("tessivum-windows-source-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Force $WorkRoot, $Evidence | Out-Null
if (Test-Path $Repo) { throw "Fresh checkout already exists: $Repo" }
Start-Transcript -Path (Join-Path $Evidence 'groups-01-15.log') | Out-Null
try {
  $principal = [Security.Principal.WindowsPrincipal]::new(
    [Security.Principal.WindowsIdentity]::GetCurrent())
  if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run as an ordinary user.'
  }
  $drive = ([IO.Path]::GetPathRoot($WorkRoot)).Substring(0, 1)
  if ((Get-Volume -DriveLetter $drive).FileSystem -ne 'NTFS') { throw 'Test path is not on NTFS.' }
  $developerMode = Get-ItemPropertyValue `
    'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' `
    -Name AllowDevelopmentWithoutDevLicense -ErrorAction SilentlyContinue
  if ($null -ne $developerMode -and [int]$developerMode -ne 0) { throw 'Developer Mode is on.' }
  if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') { throw 'Run native x64 PowerShell.' }

  rustup component add clippy rustfmt
  rustup target add x86_64-pc-windows-msvc wasm32-unknown-unknown
  Get-ComputerInfo WindowsProductName, WindowsVersion, OsBuildNumber, OsArchitecture |
    Format-List | Out-String | Set-Content (Join-Path $Evidence 'windows.txt')
  Get-Command git, rustup, rustc, cargo, bun, pnpm, node, python, pwsh |
    Select-Object Name, Source, Version | Format-Table -AutoSize | Out-String |
    Set-Content (Join-Path $Evidence 'executables.txt')
  @(
    & git --version; & rustc --version; & cargo --version; & bun --version;
    & pnpm --version; & node --version; & python --version;
    "PowerShell $($PSVersionTable.PSVersion)"
  ) | Tee-Object -FilePath (Join-Path $Evidence 'versions.txt')
  if ((& bun --version).Trim() -ne '1.4.0' -or (& pnpm --version).Trim() -ne '11.7.0') {
    throw 'Bun must be 1.4.0 and pnpm must be 11.7.0.'
  }

  git clone https://github.com/wavetao2010/tessivum.git $Repo
  git -C $Repo checkout --detach $TessivumRevision
  $actual = (& git -C $Repo rev-parse HEAD).Trim().ToLowerInvariant()
  if ($actual -ne $TessivumRevision.ToLowerInvariant()) { throw "Checked out $actual." }
  $actual | Set-Content (Join-Path $Evidence 'tessivum-revision.txt')

  New-Item -ItemType Directory -Force (Join-Path $Repo '.ci') | Out-Null
  git clone https://github.com/deepseek-ai/deepseek-harness.git "$Repo\.ci\deepseek-harness"
  git -C "$Repo\.ci\deepseek-harness" checkout 47f943859bef60e4160492346772ded9b24f765a
  git clone https://github.com/cordiverse/cordis.git "$Repo\.ci\cordis"
  git -C "$Repo\.ci\cordis" checkout 8cc9e33fab69e2d0476d126baaf2acb24e6a6ab4
  git clone https://github.com/wavetao2010/tessivum-core.git "$Repo\.ci\tessivum-core"
  git -C "$Repo\.ci\tessivum-core" checkout 86c7e1c71bd99a3c0fc70e7be6f251c89f2cc694

  $env:TESSIVUM_DEEPSEEK_VENDOR = "$Repo\.ci\deepseek-harness\vendor"
  $env:TESSIVUM_DEEPSEEK_SOURCE = "$Repo\.ci\deepseek-harness"
  $env:TESSIVUM_CORDIS_SOURCE = "$Repo\.ci\cordis"
  $env:TESSIVUM_CORE_SOURCE = "$Repo\.ci\tessivum-core"
  $env:TESSIVUM_COMPAT_HOST = "$Repo\.ci\tessivum-core\node\compat-host\src\index.ts"
  $env:CORDIS_VENDOR_ROOT = "$Repo\.ci\deepseek-harness\vendor"
  Remove-Item Env:TESSIVUM_REQUIRE_FILE_SYMLINKS -ErrorAction SilentlyContinue
  $Evidence | Set-Content "$Repo\.ci\windows-source-evidence-root.txt"

  # 01: frozen clean install, real esbuild execution, installed-state second install.
  Set-Location $env:TESSIVUM_DEEPSEEK_SOURCE
  foreach ($key in 'node-linker', 'package-import-method', 'store-dir', 'virtual-store-dir') {
    "pnpm config $key"
    pnpm config get $key
  }
  pnpm store path
  pnpm install --frozen-lockfile
  $esbuild = @(Get-ChildItem node_modules\.pnpm -Recurse -File -Filter esbuild.exe |
    Where-Object FullName -Match '[\\/]node_modules[\\/]@esbuild[\\/]win32-x64[\\/]esbuild\.exe$' |
    Sort-Object FullName -Unique)
  if ($esbuild.Count -eq 0) { throw 'No installed Windows esbuild binary found.' }
  foreach ($binary in $esbuild) { & $binary.FullName --version }
  pnpm install --frozen-lockfile
  "Group 01 exit=$LASTEXITCODE"

  # 02
  Set-Location $Repo
  python scripts/check_compat_baseline.py
  "Group 02 exit=$LASTEXITCODE"
  # 03
  python scripts/check_plugin_verification.py
  "Group 03 exit=$LASTEXITCODE"
  # 04
  python scripts/check_release_facts.py
  "Group 04 exit=$LASTEXITCODE"
  # 05
  Set-Location "$Repo\web"
  bun install --frozen-lockfile
  "Group 05 exit=$LASTEXITCODE"
  # 06
  bun run build
  foreach ($asset in 'dist\index.html', 'client-packages\bundles.json') {
    $file = Get-Item $asset
    if ($file.PSIsContainer -or $file.Length -eq 0) { throw "Invalid real Web asset: $asset" }
  }
  "Group 06 exit=$LASTEXITCODE"
  # 07
  Set-Location "$Repo\plugins\market"
  bun install --frozen-lockfile
  "Group 07 exit=$LASTEXITCODE"
  # 08
  bun run check
  "Group 08 exit=$LASTEXITCODE"
  # 09: strict mode stays unset for ordinary-user Windows; retain the visible capability result.
  bun run test
  "Group 09 exit=$LASTEXITCODE; file-symlink capability outcome is in this transcript"
  # 10
  Set-Location $Repo
  cargo check --all-targets --locked
  "Group 10 exit=$LASTEXITCODE"
  # 11
  cargo clippy --all-targets --locked -- -D warnings
  "Group 11 exit=$LASTEXITCODE"
  # 12
  cargo test --all-targets --locked
  "Group 12 exit=$LASTEXITCODE"
  # 13
  Set-Location "$Repo\web"
  bun run test:source-client
  "Group 13 exit=$LASTEXITCODE"

  # 14: first real Agent round trip.
  Set-Location $Repo
  $state = Join-Path $env:TEMP ("tessivum-windows-" + [guid]::NewGuid())
  $first = @(& cargo run --locked -- --session windows-smoke --data-dir $state `
    --replay fixtures/headless/recorded-replay.jsonl --trusted-bash 'prove the CLI tool round trip')
  $first | Tee-Object -FilePath (Join-Path $Evidence 'agent-first.stdout.txt')
  if ($first.Count -ne 1 -or $first[0] -ne 'CLI tool round trip complete: CLI_TOOL_ROUND_TRIP') {
    throw 'Unexpected first Agent stdout.'
  }
  $sessions = @(Get-ChildItem $state -Recurse -File -Filter 'session-*.jsonl')
  if ($sessions.Count -ne 1 -or $sessions[0].Length -eq 0) { throw 'Missing Session history.' }
  $sessionPath = $sessions[0].FullName
  $before = [IO.File]::ReadAllBytes($sessionPath)
  "Group 14 exit=$LASTEXITCODE"

  # 15: same state and Session; existing bytes must remain an exact prefix.
  $resumed = @(& cargo run --locked -- --session windows-smoke --data-dir $state `
    --replay fixtures/headless/recorded-replay.jsonl --trusted-bash --resume `
    'prove the CLI tool round trip')
  $resumed | Tee-Object -FilePath (Join-Path $Evidence 'agent-resume.stdout.txt')
  if ($resumed.Count -ne 1 -or $resumed[0] -ne 'CLI tool round trip complete: CLI_TOOL_ROUND_TRIP') {
    throw 'Unexpected resumed Agent stdout.'
  }
  $sessionsAfter = @(Get-ChildItem $state -Recurse -File -Filter 'session-*.jsonl')
  if ($sessionsAfter.Count -ne 1 -or $sessionsAfter[0].FullName -ne $sessionPath) {
    throw 'Resume replaced the Session file.'
  }
  $after = [IO.File]::ReadAllBytes($sessionPath)
  if ($after.Length -le $before.Length) { throw 'Resume did not append Session history.' }
  for ($i = 0; $i -lt $before.Length; $i++) {
    if ($before[$i] -ne $after[$i]) { throw "Resume replaced history at byte $i." }
  }
  Copy-Item $sessionPath (Join-Path $Evidence 'session-after-resume.jsonl')
  "Group 15 exit=$LASTEXITCODE"
  "Groups 01-15 complete. Repo=$Repo Evidence=$Evidence State=$state"
}
finally {
  Stop-Transcript | Out-Null
}
```

Group 9 must visibly identify the two file-symlink tests as executed, or name the narrowly
verified Windows file-symlink `EPERM` capability skip. Any other error fails. Junctions are
a separate capability and do not cover dangling/live file-symlink behavior. Capable
non-Windows CI sets `TESSIVUM_REQUIRE_FILE_SYMLINKS=1`, so those security tests cannot skip.

## Group 16: release Web HTTP and Ctrl+C

Open two ordinary-user PowerShell 7.4+ terminals. Run this in both to recover `$Repo` and
`$Evidence` from the same candidate checkout:

```powershell
$TessivumRevision = Read-Host 'Same tested 40-character Tessivum revision'
$Repo = Join-Path (Join-Path $HOME '测试 项目') `
  ("tessivum-" + $TessivumRevision.Substring(0, 12).ToLowerInvariant())
$Evidence = (Get-Content -Raw "$Repo\.ci\windows-source-evidence-root.txt").Trim()
Set-Location $Repo
```

Then take a pre-run process baseline in terminal B:
```powershell
$Baseline = @(Get-Process tessivum,bun,cargo,pwsh,powershell -ErrorAction SilentlyContinue |
  Select-Object -ExpandProperty Id)
```

In terminal A, start the original command interactively:

```powershell
Set-Location $Repo
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
Start-Transcript -Path (Join-Path $Evidence 'group-16-web.log')
cargo run --release -- web
```

While it listens, terminal B must receive HTTP 200 from `http://127.0.0.1:3000`, open the
same URL in a browser, and save a screenshot under `$Evidence`:

```powershell
$response = Invoke-WebRequest http://127.0.0.1:3000/ -UseBasicParsing
if ($response.StatusCode -ne 200) { throw "HTTP $($response.StatusCode)" }
$response.Content | Set-Content (Join-Path $Evidence 'web-response.html')
```

Only after HTTP and the screenshot, press **Ctrl+C once in terminal A**. Ctrl+C may end the
active script pipeline, so capture its status in the separate next prompt, then restore
fail-fast behavior:

```powershell
$WebExit = $LASTEXITCODE
$PSNativeCommandUseErrorActionPreference = $true
"Group 16 Ctrl+C exit=$WebExit" | Tee-Object -FilePath (Join-Path $Evidence 'group-16-exit.txt')
Stop-Transcript
if ($WebExit -ne 130) { throw "Expected product cancellation exit 130, got $WebExit" }
```

Exit 130 is the product's intentional Ctrl+C result (`src/bin/tessivum.rs:650-676`), but it
is not a pass by itself. In terminal B, verify the port is closed and no new related process
remains:

```powershell
Start-Sleep 2
if (Test-NetConnection 127.0.0.1 -Port 3000 -InformationLevel Quiet -WarningAction SilentlyContinue) {
  throw 'Port 3000 is still open.'
}
$leftovers = @(Get-Process tessivum,bun,cargo,pwsh,powershell -ErrorAction SilentlyContinue |
  Where-Object Id -NotIn $Baseline)
if ($leftovers.Count) { $leftovers | Format-Table Name,Id,Path; throw 'Related process remains.' }
```

## Evidence and result

Return the tested Tessivum commit, OS/build/CPU, ordinary-user/NTFS/Developer-Mode status,
all executable paths and versions (including Node), environment differences, transcript and
exit for every group, exact Market capability outcome, both Agent outputs and preserved
Session history, HTTP response, Web screenshot, Ctrl+C exit, port release, and process
cleanup. Record Chinese/space-path encoding, truncation, or file-not-found errors.

All 16 groups must complete. A conditional file-symlink skip is acceptable only for the
verified ordinary-user Windows capability failure; it is not junction coverage and does not
make any downstream `NOT RUN` group pass. Until this complete Windows 11 evidence exists,
source acceptance remains failed. Even a source pass is not package-publishing evidence.
