# Windows 原生修复验收报告：2026-09-16

> 2026-09-17 集成说明：本文保留旧 Windows 分支本机验收的原始事实。`integrate/windows-alpha29` 已接入 Alpha.29 / Core 0.1.7，但本文的 555 项 Rust、Browser、ZIP 与安装器结果不覆盖合并后的源码。草稿 PR 必须重新取得 Windows 证据。审查另发现 Windows PowerShell 5.1 的安装器经 `Environment.GetEnvironmentVariable(..., User)` 读取 PATH 会展开 `REG_EXPAND_SZ`，快照没有保存原始变量引用及注册表类型；回写与回滚可能破坏 `%JAVA_HOME%` 等动态引用。该项尚未在 Windows 执行复现或修复，是独立合并阻塞；现有 `PATH_STORE` 文件测试不覆盖注册表语义。

> 同次静态审查还发现两项 Windows 门禁：`PersistentShellInner::stop` 与最后 owner 的 `Drop` 通知 reaper 后立即终止 Job，可能在 reaper 捕获祖先关系前杀掉中间进程，遗漏逃逸孙进程；`powershell_normal_completion_reaps_its_descendant_tree` 则强制普通 `CreateProcessW` 子进程不属于任何 Job，依赖旧验证宿主的特殊行为，不能作为任意 Windows runner 的前提。这两项尚无集成后的 Windows 运行证据，不修改历史通过记录，也不据此批准合并。

## 结论与范围

**四项检查点阻塞及最终完整 Browser 复验均已通过；本机源码候选版验收收口。**

这是本机源码候选版验证，不是 Windows 正式发行。没有跳过失败测试、把命名时区改成 UTC、提升权限、启用 Developer Mode，或修改全局代理及真实用户 PATH。此前检查点 `1c4232c` 已推送至 `origin/codex/fix-windows-node-unicode-fs`；本轮后续修复尚未执行新的提交或推送，也没有发布 tag/Release。

## 四项阻塞关闭记录

| 编号 | 修复 | 验证 |
| --- | --- | --- |
| WIN-CHECKPOINT-01 | `src/schedule.rs` 与 `src/api.rs` 共用内置 IANA 数据的 `jiff`，不再依赖 Windows 不具备的 `/usr/share/zoneinfo` | 保留 `Asia/Shanghai` Browser 场景；毫秒、DST gap 拒绝、fold 取较早瞬间及 2100 年未来规则三项回归通过 |
| WIN-CHECKPOINT-02 | 新增 `web --settings-file <file>`；多个 Host 使用同一设置文件，各自保留独立数据目录和工作区锁 | 普通用户设置 Browser 场景 2/2；不使用文件符号链接、硬链接或整目录 junction；修正共享浏览器的页面所有权 |
| WIN-CHECKPOINT-03 | 正确构造 Windows 大小写不敏感的进程环境和 PATH，选择实际第一个 Bun 可执行文件 | 最终 ZIP 在无 Node PATH 下执行真实 PTC 工具往返；不是仅匹配预录助手文本 |
| WIN-CHECKPOINT-04 | 修复 PowerShell 空字节数组被管道展开后丢失 `Length` 的测试夹具；补强提交后清理失败的断言 | 最终 ZIP 的安装、升级、回滚、故障边界、重复卸载和用户数据保护行为脚本退出 0 |

## 集成中追加发现并修复

- 本地插件 `file://` URL 先前被直接去前缀，Windows 会得到 `C:///C:/...`。现在使用标准 URL 到本机路径转换；新增中文、空格、百分号及井号路径回归，真实 Legacy 包安装和退出通过。
- 发行 smoke 重启检查改用原生 RPC 的 `output.restarting`；原生命令先保留 stderr，再显式检查退出码，避免异常吞掉诊断。
- 本机 Python 的重定向标准流默认为 GBK。Python code worker 现在将输入、输出及错误流显式设为 UTF-8；中文路径下真实后代启动、超时及回收回归通过。
- 沙箱测试的 PowerShell 路径标记文件改为显式 UTF-8 无 BOM，避免中文 TEMP 被默认 ANSI 编码破坏；没有改变沙箱权限策略。
- Browser 市场夹具改用项目构建中已有的 npm 打包工具，解决 Bun 1.4.0 对中文目标路径打包失败的问题；快捷键夹具使用平台原生 Cmd/Ctrl 修饰键；工作流夹具在完成状态稳定后检查展开项，避免自动折叠与断言竞争。
- Bun `1.4.0` 在连续关闭并重启 Chrome 时曾使 DevTools pipe 报 `Could not write into pipe`；Windows Browser 夹具现在仅在新 Host 启动前终结已关闭 Bun 子进程资源。没有加入测试重试、跳过、浏览器重启或产品运行时分支。

## 本轮验证证据

证据目录：`C:/Users/Q/Documents/New project/windows-repairs-20260916`。各命令另有同名 JSON，记录参数、工作目录、时间和退出码。

| 门禁 | 实际结果 | 证据 |
| --- | --- | --- |
| Rust 全 targets、严格 Clippy、rustfmt | 同一顺序门禁退出 0；49 套件，555 passed、0 failed、0 ignored | `rust-quality-final-utf8.log`、`rust-final-totals.json` |
| 最终完整 Browser | 退出 0；`migrated web suite` 与 `remote-access.e2e.ts` 外层 2/2 通过 | `browser-final-handle-lifetime.log` |
| Web typecheck | 退出 0 | `web-types-handle-lifetime.log` |
| PowerShell 解析、兼容基线、插件账本、发行事实 | 退出 0；最终版本事实仍与保留的 Alpha.23 基准证据一致 | `source-gates-handle-lifetime.log` |
| 静态 CRT Release | `--locked --release --target x86_64-pc-windows-msvc --jobs 1`，退出 0 | `release-utf8-final.log` |
| 最终 PE 依赖 | 仅 Windows 系统 DLL，无外置 VC++ 运行库依赖 | `release-pe-final.log` |
| 实际 Windows ZIP 打包 | 退出 0，包含校验和、资源清单及打包自检 | `package-utf8-final.log` |
| 最终 ZIP 完整 smoke | 退出 0；双启动器、PowerShell 首次及恢复、无 Node PTC、市场重启、Legacy shutdown 均通过 | `package-smoke-delivery/summary.txt` |
| 最终 ZIP 安装器故障边界 | 退出 0，输出 `Windows installer behavioral tests passed` | `installer-delivery.log` |

Rust 顺序门禁：

~~~powershell
cargo test --locked --all-targets --jobs 1 -- --nocapture
cargo clippy --locked --all-targets --jobs 1 -- -D warnings
cargo fmt -- --check
~~~

Browser 命令在 `web` 执行，并用 `TESSIVUM_TEST_BINARY` 指向本轮静态 Release：

~~~powershell
bun test ./tests/migrated.test.ts ./tests/remote-access.e2e.ts --max-concurrency 1 --timeout 120000
~~~

## 失败记录与环境偏差

- 保留所有中间失败日志，包括早期时区、PTC、Legacy URL、Python Unicode、沙箱编码及 Browser 夹具失败；最终通过不抹去这些记录。
- MSVC 曾报 `LNK1140/LNK1318`。检查确认 D 盘只剩约 41 MiB；只执行本项目的 `cargo clean --package tessivum`，移除 63.7 GiB 生成物，保留源码和依赖缓存。后续 TEMP/TMP 使用 `E:/Tessivum Windows/临时 构建`；在中文空格路径下完成后续验证。
- 打包曾缺少本次准备的 jq PATH，并因失效代理无法补齐新依赖的许可证元数据；使用已有、校验过的 jq 和命令级代理覆盖解决，没有改全局设置。
- Browser 生命周期对照实验确认 Chrome 正常关闭约 150 ms，而下一次启动前显式终结已关闭 Bun 子进程资源可消除 DevTools pipe 写入失败；该 Windows 专用夹具修复已由完整 Browser 复验覆盖。临时诊断源码将删除，原始日志保留为证据。
- 主源码仍是既有全局 worktree，不是重新克隆到中文路径的一次性全新 16 组验收；不把本轮结果冒充全新机器、hosted Windows CI、签名发行或 Windows ARM64 验证。
- 前轮客户端 3182 项、Market 1056 项及真实公网 Quick Tunnel 的证据继续保留于 `windows-delivery-20260916`，本轮未把这些历史结果说成全部重新执行。原有 Market 三项跳过及旧 Node 专用场景的限制不变。

## 本地产物

- 文件：`dist/tessivum-0.1.0-alpha.27-x86_64-pc-windows-msvc.zip`。
- 大小：26,986,930 字节。
- SHA-256：`0dc5881d0ddbdbeed3ab01a75b4aaf8b417b8773fb04daa03853ec2d21301c90`；与相邻 `.sha256` 一致。
- ZIP 是包含后续修复的本地 Alpha.27 源码候选包，不是已发布 Alpha.27 的官方新增资产。归档未签名；Bun、pnpm 等运行时仍按功能要求单独提供。

## 资源清理

已按生成目录名称、创建时间、进程归属、嵌入资源哈希及无 reparse point 条件清理 C 盘的 251 个本轮临时前端资产目录，回收 4,968,614,375 字节；Browser 完成后再清理 E 盘中文空格临时根下 360 个目录，回收 7,127,947,614 字节。完整路径与哈希见 `generated-cache-cleanup-report.json`、`browser-postsuite-cleanup-report.json` 和 `browser-postsuite-cleanup-remainder-report.json`；未删除用户数据或原始失败日志。

最终进程检查未发现 `tessivum.exe`；3000、3001、3002 均没有监听端口。临时 Browser 诊断源码已删除，原始日志保留为证据。

## 2026-09-17 Alpha.29 集成：独立验收记录

以下记录与上面的 9 月 16 日历史结果分开。当前修复基线为
`011694d57480b7e030fffb0508be9960bcafd828`，开发分支为
`fix/windows-alpha29-integration`，目标为草稿 [PR #6](https://github.com/wavetao2010/tessivum/pull/6)。
产品保持 Alpha.29，Core 保持 `0caaccf9a79d7a906a08a21c3032eafebe084ffc`（0.1.7）。

本轮证据目录：`C:/Users/Q/Documents/New project/windows-integration-20260917`。
每个执行步骤保留 `.log` 和包含命令、工作目录、起止时间及退出码的 `.json`；
定向修复工作区的结果不冒充最终提交或干净机器验收。

- 工作区：`C:/Users/Q/Documents/New project/集成 修复/tessivum-alpha29`；旧工作区及未跟踪依赖未清理。
- 环境：Windows 11 Pro x64 build 26200、NTFS、非管理员、Developer Mode 未启用。
- 工具：现有 Node 24.20.0 固定安装仅加入本轮子进程 PATH；Bun 1.4.0、pnpm 11.7.0、Rust/Cargo 1.94.0、Windows PowerShell 5.1.26100.8655、Python 3.12.10。初始 PowerShell 7 探针报告 7.6.4，随后实际解析到 WindowsApps 的 7.6.6；本任务未执行 PowerShell 安装或升级，后续以 `powershell-resolution.log` 记录的实际版本为准。
- 原始用户 PATH 为 `REG_EXPAND_SZ`，测试前保存原始值的 SHA-256；没有在日常用户 PATH 上执行安装或破坏性实验。
- DeepSeek/Cordis 从已固定提交的本地 Git 仓库进行全新 `--no-local` 克隆；Core 从远端检出上述 0.1.7 pin。没有复制旧 `node_modules` 或使用旧产品 ZIP。
- 全新冻结安装、实际 esbuild 0.21.5/0.25.12/0.28.1 执行及安装后的第二次冻结安装通过。Web 冻结安装和当前源码 Web build 通过。
- Cargo 初次 fetch 因失效本地代理失败；仅本轮命令使用 Git CLI、SSH 和代理覆盖后通过。Host 模块下载遇到 TLS EOF，已有不可变缓存通过当前清单及逐文件 inventory 校验后使用；不改变版本或完整性检查。

### 基线 CI 与定向观察

- [基线 CI](https://github.com/wavetao2010/tessivum/actions/runs/35196621898)：`verify` 通过；`windows` 在 Rust tests/Legacy bridge 阶段失败，其后客户端/WASM 步骤未执行；`browser-e2e` 在 `image-input.e2e.ts:108` 等待图片能力提示时失败。
- 未修改基线上的 `powershell_normal_completion_reaps_its_descendant_tree` 本机执行通过，输出 `30348|True|False`。这仍是旧宿主特殊条件，不是普通 runner 可移植性的证明，也不是 WIN-INTEGRATION-02 已复现或关闭。
- WIN-INTEGRATION-03 已在未修改的基线上取得真实注册表 RED：进程局部 HKCU 映射到 GUID 临时键，只调用原安装器快照读取函数。PowerShell 5.1 与实际 PowerShell 7.6.6 均将 `%TESSIVUM_SNAPSHOT_REFERENCE%\bin` 展开为固定路径，而注册表原文及类型仍为变量引用和 `REG_EXPAND_SZ`。两条命令退出 1，临时键均已删除；此探针未调用真实用户 PATH 写入函数。完整写入/回滚/事务覆盖仍须另行验证。
- Browser 请求屏障实验证明：Host 尚未接收模型选择时发送，显示“发送前请先选择模型”；放行选择并等待界面显示 Text Model 后发送，才显示图片能力拒绝且保留草稿。没有使用定时 sleep 驱动该证据。测试夹具增加等待已选模型的可见状态，原完整图片场景定向通过；不能仅据此宣称原 CI 只有这一种失败原因或跨平台 CI 已通过。
- 三项安全阻塞及最终提交的完整 Rust、Browser、注册表事务、ZIP 和 CI 仍须以本轮实际结果结案；本节不授予合并或发布批准。

### 本轮源码修复与定向证据

- WIN-INTEGRATION-01：仅把受控逃逸夹具追加到未修改的 `011694d` 生产代码，最后持有者/运行时关闭场景退出 101，明确因逃逸孙进程未退出而失败；修复后取消、超时、dispose、运行时关闭四项全部通过。证据：`persistent-escape-baseline-red-qualified.log`、`persistent-escape-green.log`。
- WIN-INTEGRATION-02：普通子进程测试不再要求子进程处于所有 Job 之外；独立逃逸夹具只允许自身测试 Job 显式 breakaway，并检查该 Job 的成员身份、文件锁释放及无关进程存活。普通子进程定向复验退出 0，见 `integration-ordinary-child-green.log`；hosted Windows CI 仍待当前提交结果。
- WIN-INTEGRATION-03：旧安装器在 PowerShell 5.1 与 7 的真实安装回滚中均把 `%TESSIVUM_REGISTRY_PATH_TEST_TOKEN%` 写成已展开值，分别见 `registry-transactions-baseline-native-capture.log`、`registry-transactions-baseline-ps7-red.log`。修复后的双宿主真实注册表事务退出 0，见 `registry-transactions-green-native-capture.log`；快照/写入/恢复独立检查也退出 0。所有写入均在已验证的进程局部 HKCU 映射下进行，不是全新用户或真实用户 PATH 安装验收。
- 注册表事务此阶段使用本轮从未修改基线新构建的 Alpha.29 ZIP，配合当前安装器；不作为最终修复 ZIP 的验收结果。测试夹具修正了脚本参数与 script-scope 变量重叠、数组 splatting 未绑定 `-Uninstall` 开关、未初始化退出码及 PowerShell 5.1 原生 stderr 捕获问题。
- PowerShell 5.1 的 MAX_PATH 限制曾在 ZIP 展开前阻断测试，不能算作 PATH 缺陷的 RED。夹具缩短临时目录名称，事务命令的 TEMP/TMP 使用 `E:/验 收`；保留中文及空格条件，没有开启系统长路径策略、提权或改变全局环境。
- 当前源码 Rust 全 targets：49 个结果套件，560 passed、0 failed、0 ignored，退出 0；见 `integration-rust-all-targets.log`、`rust-integration-totals.json`。严格 Clippy、最终格式及最终提交验收另行记录。
- 当前源码固定客户端：240 文件、3182 测试全部通过，退出 0，见 `integration-source-client.log`。Market：check 退出 0；53 文件、1056 passed、3 既有 skipped，见 `integration-market-check-portable-pack.log`、`integration-market-test.log`。
- Market offline smoke 复现 Bun 1.4.0 无法在中文目的路径创建 tarball；改用已有 Browser 打包方案中的 npm 后，两次归档摘要一致，Bun 离线安装和实际 import 均通过。没有更换 Bun 版本或放宽离线检查。
- 新增注册表 CI 步骤最初打断了固定依赖 checkout 的位置约束；已移到三个 checkout 之后。兼容基线、插件账本、发行事实检查均退出 0，原失败日志保留。

### 首个候选提交及追加验收发现

- `8263d0987e12e141cb0d198a670dd31de495130f` 已快进推送至 PR #6 的 `integrate/windows-alpha29`，未修改 main、创建 tag 或发布资产。本地最终 Rust 重验为 560 passed、0 failed、0 ignored；严格 Clippy、格式、重建 WASM guest 合约及真实 Legacy Node 生命周期均退出 0。完整安装器行为检查也退出 0。
- 本轮静态 CRT release 构建、打包自检及 PE 检查通过；PE 仅列出 Windows 系统 DLL。首个 ZIP 的实际安装器完整测试通过，但 ZIP smoke 在 Legacy 本地包安装处失败，因此该包不作为最终验收通过的交付；原包摘要及结果记录在 `candidate-8263d09.json`。
- 新发现：`file://` URL 的预检已解码，但传给 pnpm 的参数仍保留百分号编码。新增真实 CLI 回归在修复前退出 101，修复后退出 0，并确认中文、空格、`%`、`#` 路径中的本地 bundle 实际安装并贡献可加载条目；见 `plugin-file-uri-real-red.log`、`plugin-file-uri-real-green.log`。没有把 ZIP smoke 改成绕过 URL 输入。
- [该提交 CI](https://github.com/wavetao2010/tessivum/actions/runs/35208687913) 的 Linux verify 已通过；macOS Browser 在 `openSeededSession` 点击已不存在的 collapsed 选择器时超时，公开 annotations 指向 `markdown-inline-code-links.e2e.ts` 和 `support.ts:676`。修复导航夹具：只在 Sessions 树中原子检查并展开关闭的分组，避免与自动展开竞争；不改产品、不增加重试或 sleep。实际 Browser 对关闭/已展开两种初态均通过，八个调用方场景全部通过，截图为 `seeded-session-disclosure.png`。
- 首次本地完整 Browser 另在 `queue-actions.e2e.ts` 遇到 Playwright 1.62.1 的 `request@… was not bound in the connection` 协议异常。根因尚未确认；没有删除 Queue 断言、吞掉此异常或增加自动重试。原始 `integration-browser-full.log` 保留，追加源码修复后须重新执行完整套件。
- Git SSH 推送可用，但现有 GitHub REST 凭据返回 401；非交互 Credential Manager 查询得到的既有凭据也返回 401。尝试连接既有 PR 浏览器标签超时，未请求导航其他用户页面。PR 正文暂不能直接改写；代码、仓库文档与公开 CI 状态仍可更新和验证，见 `pr-metadata-access.json`。
- 追加 URL 修复后的完整 Rust 重验：49 套件、561 passed、0 failed、0 ignored；严格 Clippy 与格式检查均退出 0。记录为 `integration-tests-uri.log`、`rust-uri-totals.json`、`integration-clippy-uri.log`、`integration-format-uri-check.log`。最终 scoped 侧栏夹具的两种初态和八个调用方复验也全部通过，见 `browser-disclosure-scoped-proof.log`、`browser-seeded-scoped-consumers.log`。
- `2dce9ca5f140c445800730c56cc84e87065473cf` 的静态 release 重建、打包自检、完整安装器行为检查和 ZIP smoke 均退出 0。ZIP 为 26,998,562 字节，SHA-256 `8eda214308e24f91be1c268fe9f62c145dde34bb55ee5cf7abb9936a09d46fea`；`final-zip-smoke-uri/summary.txt` 确认 PowerShell/cmd 无头启动、无 Node PTC、market Browser restart 与 Legacy shutdown 通过。安装器证据为 `integration-installer-uri-zip.log`。
- Queue 协议异常追加定位：`bun-promise-immediate-order.log` 的最小实验显示 Bun 1.4.0 Promise matcher 将新入队的 immediate 排在已入队任务之前；显式 `await` 保持 FIFO。Playwright 进程内协议使用 `setImmediate` 分发创建对象和请求事件，这解释了此前的未绑定 request 异常。将六个 Browser 文件的 30 个 `.resolves` 改为先 await、后相同断言；审批前文件检查改为明确要求 `ENOENT`，不再接受任意读取错误。定向六文件七场景、155 次断言全部通过，见 `browser-awaited-targeted.log`；不修改 Bun/Playwright 依赖或增加重试。
- 上述提交的 macOS CI 已越过 Sessions 导航，随后在 `produced-files.e2e.ts:118` 等待恰好两个按钮时超时。上游组件按实际字体度量选择可容纳的文件前缀，因此固定数量不是跨平台契约。真实浏览器放大字体后原断言超时，见 `produced-font-forced-red.log`；改为校验有序前缀、精确剩余数量及不溢出的单行布局，保留原生目录交接检查。
- `integration-browser-awaited-full.log` 另记录一次 goal reload 的 `net::ERR_NO_BUFFER_SPACE`；网络诊断时仅 443 个 TCP 连接，尚不能认定系统套接字耗尽。带 requestfailed/console URL 诊断的同场景通过，见 `goal-reload-network-diagnostic.log`。没有屏蔽该异常或把失败批次算作通过；最终完整门禁结果须另行记录。
- 正常字体及放大字体的产物摘要场景均通过：`produced-font-green.log`，2 场景、36 次断言；Browser typecheck 退出 0，见 `browser-await-typecheck.log`。
- 同轮完整 Browser 还暴露远程撤销测试把 warning 数量固定为一次：撤销后后台重连实际收到预期 401，调度不同会产生更多连接告警。删除该日志措辞/次数断言，改验同一个受保护 session.list 请求由撤销前 HTTP 200 变为撤销后 HTTP 401；保留活动 WebSocket 被关闭、远程 shutdown 被拒绝、已授权阶段零错误及撤销后零 pageerror 的检查。定向 `remote-revocation-behavior-green.log` 通过，17 次断言。最终完整套件记录为 `integration-browser-final-acceptance.log`，须以该文件实际退出状态为准。
- `ba6504c2dc2340b7c73577180c3141fdfc2d7a22` 的 [CI](https://github.com/wavetao2010/tessivum/actions/runs/35215864530) 已结束：Linux verify 与 Windows 全门禁通过；macOS Browser 失败，不能称为跨平台全绿。公开注释指向 steering 后台轮询连接失败及 scaffold 工作区会话匹配失败；完整日志需要当前不可用的 GitHub 认证，既有 PR 标签 relay 再次超时。
- Windows 上用每个动作 250ms 的真实 Browser 复现 steering：`steering-primary-error-diagnostic.log` 确认先超时等待两条队列消息，Host 在 finally 关闭后后台轮询才报连接失败。`steering-key-routing-diagnostic.log` 进一步记录第三次 Enter 落在 BODY，只有 BANANA 入队，ORANGE 留在草稿。问题面板到达会隐藏主输入框；单纯先等面板出现再输入的尝试失败，已撤回，不延长超时或重试发送。
- 最终 steering 夹具通过 Playwright connectToServer 连接真实 Host，仅暂存原始 question/requested WebSocket 帧，队列快捷键完成后原样放行并正常回答；不制造后端响应、不删除事件或降低持久化/FIFO 检查。移除原先依赖 100ms 播放节奏的窗口。现有 beforePage 钩子移动到空白页面已创建但首次导航尚未开始的位置，三个既有 RPC 初始化调用方均保留。
- Native session 创建先写入会话，再完成工作区挂接；Browser 启动就绪改为等待 blank session 的 workspaceId 已实际发布，而不是只看到 blank 即返回。原有 scaffold、冷空会话及自动选中检查均保留。`browser-gated-readiness-targeted.log`：7 文件、10 场景、147 次断言通过；`steering-gated-paced-green.log`：250ms 动作节奏下四种 steering 场景、31 次断言全部通过。追加完整 Browser 记录为 `integration-browser-gated-full.log`。
- 追加边界修复：非持久 PowerShell 取消路径现在把拥有 Windows Job 的对象移动进同一个 blocking capture/fence 任务；外层任务取消不能先触发 Job drop。新增单 worker 排队回归实际确认中间祖先、逃逸孙进程及锁资源都回收，且无关进程存活；目标套件 5 passed。
- 安装/卸载 PATH 事务在写注册表前登记 `$pathChanged`，通知组件首次 Add-Type 故障也进入原始 PATH/类型/所有权回滚。注入故障覆盖 PowerShell 5.1 与 7，完整 `RegistryPathRegression` 退出 0，输出 `Windows installer registry PATH regressions passed`；证据 `installer-notifier-rollback-final.json`。测试仅使用进程局部 HKCU，不修改真实 User PATH。
- 边界修复后的 `cargo fmt -- --check`、严格 Clippy、全 targets 编译检查均退出 0；完整 Rust 全 targets 回归待本轮最终命令返回后记录。
