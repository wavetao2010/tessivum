# Tessivum Phase 10 Windows 原生发行开发计划

> 状态：实施中（Milestone 10-A Windows CI gate）
> 计划日期：2026-09-04
> Tessivum 起点：`v0.1.0-alpha.23` / `4674aeda870989fede1fc79fb07afbe764d3a1eb`
> 产品 Core pin：`tessivum-core v0.1.6` / `640e9ea41810861eebd5bbf300052072e989259c`
> 上游兼容基线：DeepSeek Harness `0.1.0-rc.5` / `47f943859bef60e4160492346772ded9b24f765a`
> 首个正式目标：Windows 11 x86-64 / `x86_64-pc-windows-msvc`

## 1. 目标

Phase 10 的目标不是“让 Rust 在 Windows 上勉强编译”，而是从同一 Tessivum 源码发布可安装、可升级、可卸载、可运行 Web/Agent/插件链路的 Windows 原生版本：

```text
GitHub Release ZIP
  -> install.ps1 校验并安装
  -> tessivum.cmd / tsv.cmd
  -> tessivum.exe
  -> Rust Host + Browser shell
  -> PowerShell 工具 + Windows sandbox
  -> Native / WASM / Legacy Node 插件
```

只有真实 Windows runner 和干净 Windows 11 x64 环境通过构建、安装、Agent、插件、Browser、升级及进程清理门槛后，README 才能把 Windows 从 “Not published” 改成受支持。

关联文档：

- [二阶段开发计划](DEVELOPMENT_PLAN.md)：总体顺序和完成状态；
- [目标运行时架构](ARCHITECTURE.md)：Rust Host、WASM、Legacy Node 和 Browser 的所有权；
- [DeepSeek Harness 兼容基线](COMPATIBILITY_BASELINE.md)：冻结的 Browser/Wire 行为；
- [Phase 5 Agent Mode 计划](PHASE5_NATIVE_AGENT_MODES_PLAN.md)：当前只实现 Unix persistent shell 的历史边界；
- [Phase 6 Profile 兼容计划](PHASE6_DSH_PROFILE_COMPATIBILITY_PLAN.md)：pnpm、升级和 `tsv` 契约；
- [Phase 8 Remote Access 计划](PHASE8_REMOTE_ACCESS_COMPATIBILITY_PLAN.md)：Cloudflare Quick Tunnel、安全边界和进程清理契约。

如实现与本文冲突，先更新本文，再修改代码或公开支持声明。

## 2. 当前事实与阻塞项

现有代码已经包含部分 Windows 分支，但还不能形成产品闭环。

| 区域 | 当前事实 | 发布阻塞 |
|---|---|---|
| Release | `.github/workflows/release.yml` 只构建 Linux/macOS 四个 target | 没有 MSVC build、ZIP、SHA-256 或 Windows smoke |
| 打包 | `scripts/package_release.sh` 明确拒绝 Windows target，并创建 Unix symlink/launcher | Windows 不能消费现有归档布局 |
| 安装 | `install.sh` 只识别 Darwin/Linux | 没有 PowerShell 安装、PATH、升级、回滚、卸载 |
| Shell | `BuiltinTools::build` 在非 Unix 且启用 `bash` 时返回 `UNSUPPORTED_BUILTIN_BASH` | 标准 Agent Mode 无法正常启动 |
| Persistent shell | `PersistentShell`、marker protocol 和 workspace FD 全部受 `cfg(unix)` 约束 | Minimal Mode 在 Windows 不可用 |
| Sandbox | `Sandbox::local()` 只检测 macOS `sandbox-exec` 和 Linux `bwrap` | 默认 `workspace-write`/`read-only` 无 Windows 强制执行面 |
| 子进程 | `SubprocessRuntime` 的非 Unix `signal_tree` 是 no-op | cancel/timeout/shutdown 可能留下 PowerShell、工具或后代进程 |
| Legacy Node | `tessivum-node-bridge` 已有 Windows Job Object；DeepSeek Harness 也有 Windows PowerShell、ACL sandbox 和 CI | 可复用语义及测试，不必重新发明产品协议 |
| Profile mutation | `plugin_manager.rs` 直接执行裸 `pnpm` | Windows 上 `.cmd`/`.bat` shim、PATHEXT 和进程树没有统一处理 |
| 用户目录 | `cli.rs`、`api.rs`、`bridge.rs` 只读取 `HOME` | 原生 PowerShell 环境不保证设置 `HOME` |
| 原子替换 | 多个持久化路径依赖 `rename(temp, existing)`；只有 Remote Access 已有 `MoveFileExW` 分支 | Windows 上更新已有设置、凭据或文件可能失败 |
| 时区 | `api.rs` 通过 `/usr/share/zoneinfo` 验证 Browser IANA 时区 | Windows 请求会拒绝正常的 `Asia/Shanghai` 等值 |
| Remote Access | cloudflared 固定资产仅有 Linux/macOS | Windows 自动 Quick Tunnel 返回 unsupported platform |
| Browser/插件 | 当前真实 Chromium、市场、社区插件和 restart gate 只在 Unix 发行物上运行 | 无法据此声称 Windows 产品兼容 |

结论：先关闭运行时和安全阻塞，再做 ZIP/安装器。只增加 Windows matrix 会产出一个无法安全使用的 `.exe`，不能作为发布方案。

## 3. 固定决策与范围

### 3.1 首发支持面

1. 首个正式 target 只有 `x86_64-pc-windows-msvc`。
2. 支持 Windows 11 x64；GitHub `windows-2025` 是持续集成基线。
3. Windows ARM64 暂不发布。ARM64 runner、Extism/Wasmtime 依赖和完整插件链路具备独立证据后再增加；不先发布未验证 artifact。
4. 用户数据继续位于 `${TESSIVUM_HOME}`，否则为 `%USERPROFILE%\.tessivum`。不迁移到安装目录，也不创建第二套 Windows 数据格式。
5. 程序默认安装到 `%LOCALAPPDATA%\Tessivum`，不要求管理员权限。
6. Bun 和 pnpm 仍是 Web/PTC/Legacy 插件前置运行时，不打进 Tessivum ZIP，也不由安装器静默安装。
7. Shell 的外部工具名继续是 `bash`，保持冻结的 DSH Tool/Wire/Agent Mode 契约；Windows 内部执行 PowerShell。此次不做协议级重命名。
8. Windows `read-only` 和 `workspace-write` 必须有真实写限制；无法建立强制 sandbox 时 fail closed。只有用户显式选择 `danger-full-access` 才能绕过。

### 3.2 发行形状

新增两个 release assets：

```text
tessivum-<version>-x86_64-pc-windows-msvc.zip
tessivum-<version>-x86_64-pc-windows-msvc.zip.sha256
```

ZIP 根目录保持现有版本化命名，并沿用已有资源布局：

```text
tessivum-<version>-x86_64-pc-windows-msvc/
  bin/
    tessivum.cmd
    tsv.cmd
  libexec/
    tessivum.exe
  share/tessivum/
    compat-host/
    host-modules/
    plugins/
    vendor/
  share/licenses/
  LICENSE
  README.txt
```

`bin/*.cmd` 只负责相对定位资源、设置现有 `TESSIVUM_*`/`CORDIS_VENDOR_ROOT` 默认值并执行 `libexec\tessivum.exe`。它们不下载依赖、不修改用户数据、不拼接 shell 命令。所有路径必须支持空格和非 ASCII 字符。

### 3.3 明确不做

- 不在第一版增加 MSI/MSIX、Microsoft Store、winget、Chocolatey 或 Scoop manifest；
- 不维护 Windows 专属 UI、Host、Session store 或插件格式；
- 不把 Linux `.tar.gz` 改名成 Windows 包；
- 不要求 WSL，不把 WSL 作为 Windows 原生验收；
- 不捆绑 Bun、pnpm、PowerShell 或 cloudflared；cloudflared 保持按需、固定版本、校验后下载；
- 不以关闭 shell、sandbox、Legacy 插件或 Browser 场景换取绿色 Windows build；
- 首版不承诺代码签名。发布说明必须明确 SmartScreen 可能提示；有稳定签名身份和密钥托管后再增加签名流水线。

## 4. Milestone 10-A：Windows 运行时基座

### 4.1 建立持续编译门槛

在 `tessivum/.github/workflows/ci.yml` 增加独立 `windows` job，而不是把大量 PowerShell 条件塞进现有 Ubuntu job：

- runner：`windows-2025`；
- Rust：与现有 CI 同一固定 toolchain；
- target：`x86_64-pc-windows-msvc`；
- Bun/pnpm：与现有版本固定一致；
- checkout：同一 DeepSeek Harness、Cordis、tessivum-core revision；
- 执行格式检查之外的 Windows 编译、Clippy、Rust tests、Web build、Legacy bridge smoke；
- Unix 特有测试可以保留 `cfg(unix)`，但跨平台契约必须补 Windows 对照，不能简单跳过整类行为。

`tessivum-core` 同时增加 Windows CI，验证现有 Node Bridge Job Object、frame protocol、runtime loader 和 Extism 构建。只有 Core 出现真实缺口才修改 Core；若 wire/API 发生变化，先发布新的 Core revision，再更新产品精确 pin。

### 4.2 收敛平台小工具

在 `tessivum` 内增加一个小型、私有的平台模块，只承载至少被两个调用点复用的 Windows 差异：

1. **Home 目录**
   - 优先 `TESSIVUM_HOME`；
   - 通用 home helper 在 Windows 读取 `USERPROFILE`，必要时组合 `HOMEDRIVE` + `HOMEPATH`；
   - 继续接受显式 `HOME`，但不要求 PowerShell 用户自行补它；
   - `cli.rs`、`api.rs`、`bridge.rs` 和默认 mode path 使用同一 helper。

2. **可执行文件解析**
   - Windows PATH key 大小写不敏感；
   - 按 PATHEXT 搜索 `.COM/.EXE/.BAT/.CMD`；
   - `.exe/.com` 直接执行；
   - `.cmd/.bat` 经固定 `%ComSpec% /D /S /C` launch plan 执行，参数使用单一、经过边界测试的编码器；
   - 绝不把未经编码的 package specifier、路径或模型输入拼进 command line；
   - `SubprocessRuntime`、PTC Bun、pnpm mutation 和测试 fixture 复用同一解析事实。

3. **原子文件替换**
   - 把 `remote_access.rs` 已有的 Windows `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` 提升为私有共享 helper；
   - Unix 继续使用同目录 `rename` 和现有 directory sync；
   - 迁移所有“已 fsync 的临时文件替换已有持久文件”调用点，包括 Settings、Credentials、Workspace registry、Remote Access、filesystem write、message feedback 和 plugin profile 文档；
   - 仅创建新文件的 rename 不做无意义改写。

4. **Browser 时区**
   - Unix 继续使用 zoneinfo canonicalization；
   - Windows 对 `UTC` 及 Browser `Intl` 返回的安全 IANA `Area/Location` 形状做有界语法验证，不把值当文件路径；
   - 不为一个提示字段引入完整时区数据库依赖；
   - 恶意控制字符、反斜杠、`.`/`..` segment 和超长值必须拒绝。

### 4.3 基座测试

Windows runner 至少证明：

- 未设置 `HOME` 时 `%USERPROFILE%\.tessivum` 正常创建；
- `TESSIVUM_HOME` 仍有最高优先级且必须是绝对路径；
- PATH/PATHEXT 大小写、空格、Unicode 和 `.cmd` shim 正常；
- 包含 `&`、`%`、`!`、引号和空格的 argv 不发生参数注入或截断；
- Settings、Credentials、Workspace、Remote Access 和工具写文件可连续覆盖两次；
- 失败替换保留旧文件，并清除本次临时文件；
- `Asia/Shanghai`、`America/Los_Angeles`、`UTC` 通过，越界或路径穿越值拒绝。

## 5. Milestone 10-B：PowerShell、Sandbox 与进程树

这是 Windows 发布的安全关键路径，必须先于打包完成。

### 5.1 PowerShell executor

保留 Tool 名 `bash` 和现有 JSON schema，将执行器按平台选择：

- Unix：保持 `/bin/sh -lc` 和 `/bin/sh -s`；
- Windows：优先 PowerShell 7 `pwsh.exe`，再回退系统 `powershell.exe` 5.1；
- 参数固定为 `-NoLogo -NoProfile -NonInteractive -Command <one argv>`；
- 每次命令前设置 UTF-8 stdout/stderr 编码，避免 PowerShell 5.1 OEM code page 破坏中文；
- 保持 stdout/stderr 上限、后台 job、取消、超时、exit code 和 tool-result 形状；Windows 没有 POSIX signal 时 `signal=null`，终止原因仍由现有 `termination` 字段表达；
- `NO_COLOR=1`、`PAGER=cat`、`GIT_PAGER=cat` 等模型友好环境继续生效；
- cwd 从当前 Session 的 durable workspace lease 解析，不接受模型提供的相对逃逸路径。

不新增一个 `powershell` Tool，也不复制 Agent Mode。Browser 和模型继续看到同一个兼容 Tool。

### 5.2 Persistent PowerShell

把 `PersistentShell` 的通用 ownership、串行化、bounded capture、cancel/timeout 和 marker parser 从 `cfg(unix)` 中提出来，只保留平台相关 spawn/frame：

- Windows 通过 stdin 驱动一个长期 PowerShell 进程；
- 每个命令写入随机 nonce 的 stdout/stderr completion marker；
- marker 不能被普通输出前缀、分块 UTF-8 或伪造旧 nonce 提前完成；
- `$LASTEXITCODE`、PowerShell 成功状态和 parse/runtime error 映射为一个稳定整数 exit code；
- Session disable、workspace lease 失效、取消、超时或 Host shutdown 必须销毁整个 Job；
- Minimal Mode 在 Windows 保持变量、函数和 cwd 的跨调用状态，不降级成伪 persistent 的逐次新进程。

### 5.3 Windows 进程树

在产品 crate 内复用一个私有 Windows Job Object owner；不改变 Cordis/Core wire：

- spawn 后立即创建并绑定 Job Object；
- 设置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`；
- timeout/cancel/shutdown 调用 `TerminateJobObject`；
- direct child 自然退出时，在上报 done 前关闭 Job 并终止仍存活的 descendants；
- Job 创建或绑定失败时，受管理命令不得以“未受管”方式继续运行；
- `SubprocessRuntime`、PowerShell、pnpm、PTC worker、cloudflared 和直接拥有 Child 的 Host 路径统一采用这条机制；
- Legacy Node Bridge 继续使用 `tessivum-node-bridge` 现有 Job Object，不建立跨仓库公共抽象。

Windows 没有可靠通用 SIGTERM 等价物。首版直接终止 Job，不伪造优雅信号；需要协议级 graceful shutdown 的 Bun Legacy Host 仍先走已有 bridge close/dispose，超时后才强制终止 Job。

### 5.4 Windows ACL sandbox

按照已固定 DeepSeek Harness `dsh-sandbox-windows-acl` 的语义，在 Rust 侧实现最小必要 Win32 后端；不采用“只改 cwd”或 PowerShell ExecutionPolicy 冒充 sandbox：

- `read-only`：创建 write-restricted token，不授予 workspace/temp 写 capability；
- `workspace-write`：以 canonical workspace 派生稳定 SID，只给批准的 workspace roots 添加写 ACE；
- 每个 Session/命令使用独立 private temp SID 和 temp root，兄弟 Session 不能互写；
- `danger-full-access`：只在现有显式权限/approval 路径通过后执行原 argv；
- Restricted token、DACL、Job Object、stdio inheritance 和 child creation 任一步失败都在 spawn 前 fail closed；
- workspace 必须位于支持所有者 DACL 的文件系统；FAT、只读卷、无权修改 DACL 的网络目录给出稳定 `SANDBOX_UNAVAILABLE`，不自动裸跑；
- canonical handle/path、reparse point、workspace/temp 相交、owner 和 exact ACE 均需验证；
- workspace ACE 可按上游语义复用；private temp ACE 在正常 dispose 时撤销，异常退出留下的受控临时目录由下次启动按 owner marker 清理；
- Windows sandbox 只承诺写限制；读取、网络和进程可见性边界必须与上游限制一同写入安全文档。

为保留当前 `SandboxPlan.argv` seam，优先使用同一 `tessivum.exe` 的隐藏内部 runner 子命令：主 Host 只生成结构化、有界参数，runner 建立 token/DACL/Job 后执行目标 argv。除非该路径无法保留 inherited stdio 和清理契约，否则不增加第二个 helper executable。

### 5.5 安全与行为测试

使用真实 NTFS 临时 workspace 和真实 PowerShell 进程验证：

| 场景 | 必须结果 |
|---|---|
| workspace-write 写 workspace | 成功 |
| workspace-write 写 Session private temp | 成功 |
| workspace-write 写 sibling workspace/temp | 拒绝 |
| workspace-write 写 `%USERPROFILE%` 任意普通位置 | 拒绝 |
| read-only 写 workspace/temp | 拒绝 |
| danger-full-access 未批准 | 在 spawn 前拒绝 |
| danger-full-access 已批准 | 按用户选择执行 |
| 中文 stdout/stderr | UTF-8 无乱码 |
| background job kill | PowerShell 及孙进程全部退出 |
| timeout/cancel/Host shutdown | 无残留 PID、pipe、temp authority |
| persistent shell | 变量/函数/cwd 保持，Session 间隔离 |
| DACL/Job API 故障 | fail closed，无未限制 child |

此部分完成后进行一次安全专项 review；不能仅以 compile/test 通过代替 restricted-token 和越界写探针。

## 6. Milestone 10-C：Web、插件与 Remote Access 闭环

### 6.1 PTC、pnpm 和 Legacy Node

1. Bun 通过统一 Windows executable resolver 找到 `bun.exe`；PTC `run_code` 完成一次真实 tool call。
2. 所有 pnpm 调用通过同一 launch plan；覆盖 fresh install、add、update、remove、rollback、超时和 Host shutdown。
3. Windows 归档不创建需要 Developer Mode/管理员权限的 symlink：
   - `vendor/node_modules/@deepseek-ai/*` 使用复制后的真实目录；
   - `host-modules/node_modules/@deepseek-ai/*` 使用复制后的真实目录；
   - `tsv` 使用 `.cmd` launcher，不复制第二份 Rust binary。
4. `TESSIVUM_COMPAT_HOST`、`TESSIVUM_HOST_MODULE_ROOT` 和 `CORDIS_VENDOR_ROOT` 在带空格安装目录下解析正确。
5. `tessivum-node-bridge` 的 Windows Job Object 覆盖正常 dispose、protocol failure、startup timeout 和 Host crash。
6. 从 packaged first-party market 安装并激活；再用当前 verified 社区样本 `dsh-better-sidebar@0.16.1` 关闭真实 Legacy/Browser 路径。

插件 build script 权限、Profile rollback、精确版本和兼容状态继续使用现有权威逻辑，不为 Windows 建第二套 package manager 状态。

### 6.2 Web Host 和 Browser

Windows 原生 Host 必须通过真实 Chromium：

- clean data root 启动 `tessivum web`；
- 建立 Workspace 和 Session；
- replay LLM 完成文本、工具调用和最终回答；
- PowerShell 工具写入 marker 文件，Browser 显示 terminal result；
- PTC Mode 完成一次 Bun tool round trip；
- first-party market 与 Better Sidebar 页面、HTTP 和 WebSocket 可用；
- Settings/Credentials 写入后 restart 恢复；
- Host-owned restart 释放 listener 后只启动一个 replacement process，boot ID 改变；
- shutdown 后 Tessivum、PowerShell、Bun、pnpm、cloudflared 和插件 descendants 均无残留；
- `pageerror=[]`、受监控 `console.warn/error=[]`、无未预期 4xx/5xx。

发布门槛运行完整 69 个 migrated Browser 场景。日常 PR 可另设较短 Windows smoke，但不能用短 smoke 替代发版全量场景。

### 6.3 Cloudflare Quick Tunnel

为 `x86_64-pc-windows-msvc` 增加当前固定 cloudflared 版本的官方 `cloudflared-windows-amd64.exe` asset 和真实 SHA-256：

- 缓存文件名保留 `.exe`；
- 下载使用 HTTPS、固定 release 和 checksum，校验失败不执行；
- PATH 上已有 `cloudflared.exe` 时保持现有优先级；
- 进程加入 Job Object，disable/restart/shutdown 无残留；
- 默认仍关闭 Remote Access；
- deterministic tests 使用 fixture，release candidate 另跑一次真实 Quick Tunnel、pair、revoke、shutdown 场景。

## 7. Milestone 10-D：ZIP 与 PowerShell 安装器

### 7.1 打包

保持一个 release payload 真相：扩展现有 `scripts/package_release.sh` 的 Windows 分支，而不是复制整份许可证、provenance 和 market 校验逻辑。

Windows 分支负责：

- 接受 `x86_64-pc-windows-msvc` 和 `tessivum.exe`；
- 验证 binary `--version` 与 tag 一致；
- 复制同一 Compat Host、host modules、Cordis vendor、market artifact/source/checksum 和许可证；
- 用真实目录替代归档内 symlink；
- 生成 CRLF、路径安全的 `tessivum.cmd` / `tsv.cmd`；
- 生成 ZIP 及相邻 SHA-256；
- 解压后复验 root、inventory、launcher 和 binary version；
- 在带空格和中文的目录中运行 launcher smoke。

发布 binary 优先使用静态 MSVC CRT，避免要求用户另装 Visual C++ Redistributable。最终以干净 Windows 11 VM 的 dependency/startup 结果为准；若某个 native dependency 阻止静态 CRT，必须明确打包其受许可的 runtime，而不是让启动时报缺 DLL。

### 7.2 `install.ps1`

新增仓库根目录 `install.ps1`，行为与 `install.sh` 对齐但使用 Windows 原生惯例：

- 用法：`powershell -ExecutionPolicy Bypass -File .\install.ps1 [version]` 和 `-Uninstall`；
- 默认安装根：`%LOCALAPPDATA%\Tessivum\versions\<version>`；
- 稳定命令目录：`%LOCALAPPDATA%\Tessivum\bin`；
- 只下载 HTTPS GitHub Release ZIP 和相邻 `.sha256`；
- 使用 `Get-FileHash -Algorithm SHA256` 校验 hash 和目标文件名；
- 使用 .NET `ZipArchive` 先枚举并验证单一 archive root、绝对路径、盘符、UNC、`..` 和 Zip Slip，再解压到同卷 staging；
- 验证 `bin\tessivum.cmd`、`bin\tsv.cmd`、`libexec\tessivum.exe` 和 `--version` 后再发布版本目录；
- 通过 staging + rename 更新稳定 launchers；任一步失败恢复旧 launcher/版本；
- 幂等重装同一版本；允许显式升级和回滚；
- 只替换带 Tessivum managed marker 且目标位于自身 install root 的 launcher；遇到外部同名文件时拒绝；
- 将稳定 bin 目录去重加入 User PATH，不修改 Machine PATH，不申请管理员权限；提示新终端读取新 PATH；
- 卸载只删除 managed launchers、版本目录和由安装器添加的精确 User PATH entry；
- 卸载保留 `%USERPROFILE%\.tessivum`，并明确给出用户自行删除数据的命令；
- 测试 fixture URL、install root、bin root 和 PATH store 只能在显式 installer test mode 下覆盖。

不使用 `Invoke-Expression` 执行下载内容。README 的一行安装命令先下载固定 tag 的 `install.ps1`，再执行本地文件。

### 7.3 安装器测试

新增无 Pester 依赖的 `scripts/test_install.ps1`，用普通 PowerShell assertion 覆盖：

- x86-64 target/URL 选择；
- fresh install、同版本重装、upgrade、downgrade；
- checksum 错误、错误 archive root、Zip Slip、缺失 executable；
- launcher collision 和部分 launcher 更新失败；
- 失败升级继续运行旧版本；
- User PATH 去重、保留无关 entry、卸载只删除自身 entry；
- `tessivum`/`tsv` 同版本输出；
- 安装路径包含空格和中文；
- uninstall 幂等，用户数据保留。

fixture 不触达真实 GitHub，也不污染 CI runner 的真实 User PATH。

## 8. Milestone 10-E：Release 流水线与公开支持

### 8.1 Release workflow

在 `.github/workflows/release.yml` 增加独立 `build-windows` job：

1. checkout 固定产品、Core、Cordis、DeepSeek Harness；
2. 下载现有 `web-assets`、`host-modules`、`cordis-runtime` 和 `tessivum-market` artifacts；
3. MSVC release build；
4. Windows ZIP 打包和 checksum；
5. 从 ZIP 解压后的 bits 执行 version、launcher、Web、PowerShell、PTC、market、Legacy plugin、restart 和 cleanup smoke；
6. 上传 `tessivum-x86_64-pc-windows-msvc` workflow artifact。

`release` job 同时 `needs: [build, build-windows]`，合并下载后：

- Linux runner 校验 Unix SHA-256；
- Windows job 已校验 ZIP，发布 job再次按 checksum 文本核对文件；
- `gh release upload` 显式包含 `dist/*.zip`；
- Homebrew Formula 仍只消费现有 macOS/Linux 四 target，不把 Windows ZIP 塞入 Formula。

### 8.2 文档与版本

所有门槛通过后才更新：

- `README.md` / `README.zh-CN.md`：Windows x64 下载链接、PowerShell 安装、manual ZIP、Bun/pnpm、数据目录、sandbox/NTFS 和 SmartScreen 限制；
- `docs/ARCHITECTURE.md`：PowerShell executor、Windows Job Object、ACL sandbox 和安装布局；
- `docs/COMPATIBILITY_BASELINE.md`：Windows Browser/Wire 验证环境，不改变冻结 DSH 协议版本；
- `docs/DEVELOPMENT_PLAN.md`：Phase 10 完成状态；
- `CHANGELOG.md`：Windows runtime、installer、security、known limitations；
- 旧文档中的“四平台”保留为历史发布事实，不做全仓文本替换；当前说明改为“五个发行 target”或精确列出四个 Unix + 一个 Windows。

版本号最后修改；Phase 10 不预先占用一个发布号，也不发布只有 `.exe --version` 能运行的部分版本。

## 9. 实施顺序

严格按风险依赖排序：

1. 增加 Windows CI compile/test job，收集真实编译失败；
2. 修复 Home、PATHEXT/launch plan、原子替换和时区；
3. 为产品通用 Child owner 增加 Windows Job Object；
4. 实现 PowerShell one-shot/background；
5. 实现 Windows ACL sandbox 并完成安全探针/review；
6. 实现 Persistent PowerShell 和 Minimal Mode；
7. 关闭 Bun/PTC、pnpm、Legacy Bridge、market、Better Sidebar 路径；
8. 增加 Windows cloudflared asset 和 Remote Access 场景；
9. 扩展 release payload，生成 ZIP/launchers；
10. 实现并验证 `install.ps1`；
11. 运行完整 Windows Rust、Browser、插件、安装、升级、回滚和进程清理门槛；
12. 更新当前文档和 Changelog，最后确定版本并发布；
13. 发布后从公开 GitHub Release 重新下载 ZIP，在干净 Windows 11 x64 环境重复 checksum、install、Web、插件和 uninstall smoke。

不得先发布 ZIP 再补 sandbox、process cleanup 或安装器。

## 10. 仓库与文件影响图

| 工作 | 主要位置 | 说明 |
|---|---|---|
| 平台 helper | `src/platform.rs`（或现有最小相邻模块）、`cli.rs`、`api.rs`、`bridge.rs` | Home、launch plan、atomic replace、Windows path facts |
| 进程树 | `src/subprocess.rs`、`plugin_manager.rs`、`code_runtime.rs`、`cloudflare_tunnel.rs` | Job Object owner；不改变 wire |
| Shell | `src/builtin_tools.rs`、`src/subprocess.rs` | 外部 Tool 仍名为 `bash` |
| Sandbox | `src/sandbox.rs`、Windows-only 子模块、CLI 隐藏 runner | restricted token、DACL、Job、fail closed |
| Legacy/插件 | `src/plugin_manager.rs`、packaged Compat Host/vendor | pnpm、无 symlink payload、真实插件 smoke |
| Remote | `src/cloudflare_tunnel.rs` | Windows asset、`.exe` cache、cleanup |
| 打包 | `scripts/package_release.sh`、`tests/release_package.rs` | 同一 payload 校验，Windows ZIP 分支 |
| 安装 | `install.ps1`、`scripts/test_install.ps1` | 无管理员权限、checksum、rollback、PATH |
| CI/Release | `.github/workflows/ci.yml`、`.github/workflows/release.yml` | Windows build、E2E、artifact、publish |
| 文档 | README 中英文、Architecture、Compatibility、Development Plan、Changelog | 只在门槛通过后改支持声明 |
| Core | `tessivum-core` CI；必要时修复 node bridge | 现有 Job Object 优先复用，不预设协议变更 |

不创建 Windows 专属产品 crate。Windows-only FFI 使用 `cfg(windows)` 和 target-specific dependency；Unix build 不链接 Win32 库。

## 11. 强制验证矩阵

### 11.1 Source gate

- Windows `cargo fmt --check`；
- Windows `cargo clippy --all-targets -- -D warnings`；
- Windows `cargo test --all-targets`；
- tessivum-core Windows workspace/bridge/Extism tests；
- Web build、source client regression 和 first-party market check/test；
- 无新的非必要依赖、重复 launcher 逻辑或第二套 Host truth。

### 11.2 Runtime gate

- Headless replay：文本 -> PowerShell tool call -> tool result -> final text；
- Standard/PTC/Minimal/Composition Mode 可创建、切换、重启恢复；
- Native、真实 Rust WASM guest、Legacy Node 三 runtime 各完成一次 activate/call/dispose；
- Settings、Credentials、Session、Workspace、attachment 和 mode 数据重启保持；
- Ctrl+C、API shutdown、Host restart、cancel、timeout 后无 descendants。

### 11.3 Sandbox gate

- 真实越界写拒绝矩阵全部通过；
- DACL、restricted token、reparse point、private temp、Job failure 都 fail closed；
- 安全 review 无未关闭高/中严重度问题；
- 文档准确说明只限制写入，不虚构读取/网络隔离。

### 11.4 Browser/插件 gate

- 69/69 migrated Chromium 场景通过；
- packaged first-party market 可安装、更新、卸载；
- `dsh-better-sidebar@0.16.1` 真实 Browser UI/HTTP/WebSocket 通过；
- Profile mutation 失败精确回滚；
- restart 后插件状态、Session、Settings 和 Credentials 保持；
- Browser 无 pageerror、受监控 console 错误或未预期 HTTP 失败。

### 11.5 发行/安装 gate

- ZIP inventory、license、provenance、market hash、binary version 完整；
- SHA-256 错误必拒绝；
- clean install、idempotent reinstall、upgrade、rollback、uninstall 全部通过；
- PATH 与 collision 处理不覆盖用户文件；
- 带空格/中文路径通过；
- ZIP 和 installer 消费同一 release bits；
- 干净 Windows 11 x64 无 Visual Studio/源码 checkout 环境可启动；
- 发布后公开 asset 重下载验证通过。

## 12. 完成定义

Phase 10 只有在以下全部满足后才能标记完成：

1. GitHub Release 发布 `x86_64-pc-windows-msvc.zip` 及相邻 SHA-256；
2. `install.ps1` 从公开 release 完成无管理员权限安装、升级、回滚和卸载，且保留用户数据；
3. `tessivum` 与 `tsv` 在新 PowerShell 终端可直接运行；
4. Standard、PTC、Minimal 和 Composition Mode 在 Windows 原生 Host 上通过各自关键链路；
5. PowerShell shell、background jobs、persistent shell、cancel/timeout 和进程树清理可观察且无残留；
6. `read-only`/`workspace-write` 的 Windows ACL sandbox 真实拒绝越界写，任何初始化失败都不裸跑；
7. Native、WASM、first-party market 和固定 Legacy 社区插件通过 packaged artifact 的真实 Browser 验证；
8. Remote Access 默认关闭，启用后的 cloudflared 下载、配对、撤销和关闭通过；
9. 完整 Windows Rust/Browser/installer/release 门槛持续运行；
10. README 中英文、架构、兼容、安全和 Changelog 与实际支持面一致。

在此之前，Windows 仍保持 “Not published”，不能用 `[INFERENCE]` 或上游 DeepSeek Harness 的 Windows CI 替代 Tessivum 自身证据。

## 13. 风险登记

| 风险 | 控制 |
|---|---|
| 只有 MSVC 编译成功，产品启动即失败 | CI 从 binary 扩展到真实 Web/Agent/plugin/archive smoke |
| shell 为赶进度裸跑 | ACL sandbox fail closed；danger 模式必须显式批准 |
| DACL/reparse point 绕过 | canonical ownership、exact ACE、真实越界探针、安全 review |
| cancel 只杀 direct child | Job Object kill-on-close，完成前处理 descendants |
| `.cmd` 参数注入 | 单一 launch plan/encoder，特殊字符和 Unicode 测试，不拼接原始输入 |
| Windows rename 破坏持久化 | 共享 MoveFileExW replace，连续覆盖和失败恢复测试 |
| 安装器 Zip Slip/碰撞覆盖 | 预枚举路径、单一 root、managed marker、staging/rollback |
| ZIP 在开发机可用、干净机器缺 DLL | 静态 CRT 优先，clean VM dependency/startup gate |
| 归档 symlink 需要管理员权限 | Windows payload 使用真实目录和 `.cmd` alias |
| pnpm/Bun 版本漂移 | 与现有 CI 固定版本一致，README 明确最低版本，release smoke 使用固定版本 |
| Windows sandbox 只限写却被宣传为全面隔离 | 安全文档明确 read/network/process visibility 不受限 |
| Windows ARM64 被误认为支持 | 下载表只列 x86-64；ARM64 保持 Not published |
| 历史“四平台”文档被错误改写 | 历史记录不动；当前 inventory 精确列举五个 target |
| SmartScreen 阻碍首次运行 | 明确无签名状态和 checksum；有可信签名链后单独实施 |
