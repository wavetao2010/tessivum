# Windows source test result for 02c5987

This report records the native Windows 11 x64 source acceptance run for commit
`02c5987e716729edd235cbd90c834eba24065419`. The run was performed on
2026-09-08 as an ordinary user, on NTFS, with Developer Mode disabled, and in a
checkout path containing Chinese characters and a space.

Overall result: **FAIL**. The candidate's `docs/WINDOWS_SOURCE_TEST.md` requires
Groups 01-15 to run fail-fast. The first clean frozen install in Group 01 exited
with code 1, so Groups 02-16 are `NOT RUN`. Results from the earlier run were
not reused and an unrun group is not treated as passing.

## Test basis

The run followed `docs/WINDOWS_SOURCE_TEST.md` from the candidate commit while
retaining the scope constraints from the earlier test instructions. In
particular, the procedure requires fresh evidence, an immediate stop after the
first installation failure, and preservation of the failed checkout without a
cleanup or blind retry. Group 16 may run only after Groups 01-15 complete.

This was a source test. It did not test a release ZIP, `install.ps1`, upgrade,
uninstall, Linux `.tar.gz`, package build, or package publication. The codeload
ZIP described below was used only as an exact-commit source acquisition
fallback; it was not tested as a Windows release package.

## Candidate source

The GitHub commit API returned the exact requested SHA.

| Item | Value |
| --- | --- |
| Requested commit | `02c5987e716729edd235cbd90c834eba24065419` |
| Commit title | `Merge pull request #4 from wavetao2010/feat-windows-runtime-10b` |
| Commit description | `Implement Phase 10-B Windows runtime and ACL sandbox` |
| Commit timestamp | `2026-09-07T13:36:16Z` |
| Source ZIP size | 5,347,425 bytes |
| Source ZIP SHA-256 | `4BA4905EA556CDA6CAE2D608BD7FD0BBAA69C412F94568808EA5F88008CC5A42` |

### Git acquisition deviation

The primary repository could not be cloned through Git during the test. The
configured local proxy was not listening. After clearing the proxy only for the
test process and selecting Schannel and HTTP/1.1, Git and Windows curl still
timed out while connecting to `github.com` at `20.205.243.166:443`. Cloudflare
and Google DNS-over-HTTPS returned the same address. PowerShell successfully
retrieved the commit API response and exact-SHA codeload archive with HTTP 200.

The source was therefore obtained from the exact-SHA codeload archive and
validated against both the GitHub commit API and the recorded SHA-256 digest.
This does not satisfy the procedure's requirement for a real Git checkout of
the primary repository and remains an environment deviation. No global Git,
proxy, or DNS configuration was changed.

## Pinned dependencies

All three pinned dependencies were freshly downloaded at their required SHAs.
Their SHAs were also checked against retained Git metadata from the previous
run. Before Group 01, each dependency was at the expected revision with a clean
tracked worktree, and DeepSeek did not contain `node_modules`.

| Dependency | Revision | Source ZIP SHA-256 |
| --- | --- | --- |
| DeepSeek Harness | `47f943859bef60e4160492346772ded9b24f765a` | `CB275F9D775DB13A3EFDB90E6942438C9FEB2096629197D253CCBD36F4A770DB` |
| Cordis | `8cc9e33fab69e2d0476d126baaf2acb24e6a6ab4` | `14A875E37DA2A4ED98E15195BDE7DE145F2122978734A2A5ED3B67FFB82F0FF6` |
| tessivum-core | `86c7e1c71bd99a3c0fc70e7be6f251c89f2cc694` | `4B0B00E0352C0A09946CE65AC8849EFE088141D4F3E9A50CC40DA10870B0F744` |

The tessivum-core GitHub archive used LF line endings while the local Git
configuration used `core.autocrlf=true`. Attaching the retained Windows
checkout metadata directly to that archive reported 96 EOL-only modifications;
`git diff --ignore-space-at-eol --quiet` returned 0. The actual `.ci` dependency
used the retained clean CRLF checkout at the same SHA. No dependency source was
reset or manually patched.

## Test environment

These values were captured during this run and were not copied from the earlier
report.

| Item | Actual value |
| --- | --- |
| Windows | Windows 11 Pro, 10.0.26200, build 26200 |
| Architecture | AMD64, 64-bit |
| CPU | Intel Core i7-14700KF, 20 cores, 28 logical processors |
| File system | `C:`, NTFS |
| Privilege | Ordinary user, not administrator |
| Developer Mode | Registry value absent; treated as disabled |
| PowerShell | 7.6.4 Core |
| Git | 2.51.2.windows.1 |
| Rust | rustc 1.94.0, cargo 1.94.0, stable MSVC |
| Rust targets | `x86_64-pc-windows-msvc`, `wasm32-unknown-unknown` |
| Rust components | `clippy`, `rustfmt` installed |
| Bun | 1.4.0 |
| pnpm | 11.7.0 |
| Node.js | 24.11.1 |
| Python | 3.12.10 |

The candidate environment snippet calls `Get-ItemPropertyValue` with
`-ErrorAction SilentlyContinue` for the Developer Mode value. In the global
stop-on-error scope on this machine, the absent value still caused a terminating
exception. A read-only `Get-ItemProperty` query was then used to repeat the
environment gate, confirming that the value was absent. The registry was not
modified.

## Group 01 result

Group 01 was attempted exactly once with PowerShell native-command error
handling enabled. The run stopped as soon as the first installation command
returned non-zero.

| Item | Result |
| --- | --- |
| Command | `pnpm install --frozen-lockfile` |
| pnpm | 11.7.0 |
| Exit code | 1 |
| Duration | 151.6 seconds |
| Workspace scope | 238 projects |
| Lockfile | Frozen; supply-chain policy check passed |
| `node-linker` | `undefined` |
| `package-import-method` | `undefined` |
| `store-dir` | `undefined` |
| `virtual-store-dir` | `undefined` |
| Store path | User-local pnpm store v11 |
| Package progress | 923 resolved, 917 reused, 915 added |
| Failed package | `esbuild@0.28.1` |
| Error | `ERR_PNPM_EPERM` |

The complete terminal error identifies a rename from the newly created
temporary directory to the final package directory:

```text
esbuild_tmp_13744_16 -> esbuild
EPERM: operation not permitted, rename
```

The failure occurred in a fresh source directory with a fresh temporary
directory name. It reproduces the error retained from the 2026-09-07 run of
candidate `630fe949cb8908665ca6d0bad5ba6410a6bc4446`; it cannot be reported as
fixed or not reproduced.

The failed state was preserved as required:

- The destination `esbuild/package.json` did not exist.
- The temporary `esbuild_tmp_13744_16/package.json` existed.
- The DeepSeek tracked worktree remained clean.
- `node_modules` was not cleaned.
- Installation was not attempted a second time.
- No `esbuild.exe` was executed.
- Group 01 was not retried.

The Group 01 invocation assigned `TESSIVUM_COMPAT_HOST` from a misspelled
PowerShell variable named `$reporiel`. The only executed step was pnpm install,
which does not read that variable, so the typo did not cause this installation
failure. It would need correction before any later group could run. The typo is
preserved in the original transcript.

## Group status

The required fail-fast behavior produced the following final status.

| Group | Status | Reason |
| ---: | --- | --- |
| 01 | **FAIL, exit 1** | DeepSeek esbuild rename `EPERM` |
| 02 | `NOT RUN` | Group 01 fail-fast |
| 03 | `NOT RUN` | Group 01 fail-fast |
| 04 | `NOT RUN` | Group 01 fail-fast |
| 05 | `NOT RUN` | Group 01 fail-fast |
| 06 | `NOT RUN` | Group 01 fail-fast |
| 07 | `NOT RUN` | Group 01 fail-fast |
| 08 | `NOT RUN` | Group 01 fail-fast |
| 09 | `NOT RUN` | Group 01 fail-fast; symlink skip not evaluated |
| 10 | `NOT RUN` | Group 01 fail-fast |
| 11 | `NOT RUN` | Group 01 fail-fast |
| 12 | `NOT RUN` | Group 01 fail-fast |
| 13 | `NOT RUN` | Group 01 fail-fast |
| 14 | `NOT RUN` | First Agent round trip not executed |
| 15 | `NOT RUN` | Agent resume and Session prefix not verified |
| 16 | `NOT RUN` | Groups 01-15 incomplete; Web service not started |

The Rust WASM guest rebuild and contract checks, and the real Legacy Node
lifecycle checks added to the Windows CI, occur downstream from Group 01. They
were not executed locally and no result can be inferred for them.

## Web, port, and process checks

Group 16 was not authorized to run, so this run produced no Web HTTP response,
browser page, screenshot, or Ctrl+C exit-code-130 evidence. The screenshot from
the earlier run was not reused.

A final read-only audit confirmed that port 3000 was free and that no related
`tessivum.exe`, `bun.exe`, `node.exe`, `cargo.exe`, `pwsh.exe`, or
`powershell.exe` process remained. The incomplete Group 01 `node_modules` tree
was left intact for reproduction. No Agent Session state directory was created.

## Evidence retention

Fresh raw evidence is retained locally by the tester and is intentionally not
committed to the repository. The retained evidence set contains:

| File | Contents |
| --- | --- |
| `environment.txt` | Windows, CPU, privilege, Developer Mode, and filesystem |
| `executables.txt` | Resolved executable paths |
| `versions.txt` | Tool versions |
| `source-provenance.txt` | Candidate API, source, dependency SHAs, and hashes |
| `group-01.log` | PowerShell transcript |
| `group-01-native-output.log` | Complete output and stderr from the single pnpm run |
| `group-01-failure-state.txt` | Preserved esbuild failure state |
| `group-status.txt` | Group 01-16 status |

`Start-Transcript` did not capture pnpm's native progress and stderr completely;
it recorded only `NativeCommandExitException`. The transcript was retained, and
the complete native output from the same single execution was stored separately
in `group-01-native-output.log`. Installation was not rerun to supplement logs.

## Acceptance decision

Commit `02c5987e716729edd235cbd90c834eba24065419` did not pass native Windows 11
ordinary-user source acceptance on this machine. The direct cause was the first
clean Group 01 install reproducing pnpm's `esbuild_tmp_* -> esbuild` rename
`EPERM`. In addition, the primary source could only be acquired through the
exact-SHA codeload fallback because of the GitHub network route, so the real Git
checkout requirement was not met during the test.

A new clean candidate must first resolve or explain the repeatable Group 01
installation failure under the same ordinary-user, Developer Mode disabled,
Chinese-and-space NTFS path conditions. Only after its first frozen install
succeeds should testing restart from Group 01 and proceed to collect fresh
evidence for Groups 02-16.
