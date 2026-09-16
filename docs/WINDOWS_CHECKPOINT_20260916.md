# Windows 原生交付检查点：2026-09-16

## 结论与范围

**检查点，不是 Windows 正式发行；完整 Windows 验收仍未通过。**

按用户要求，停止扩大剩余问题的修复范围，保留失败测试与日志，推送当前修复分支。没有将失败项改为跳过，没有把非 UTC 时区测试替换成 UTC 来掩盖问题，也没有发布 Release/tag、推送 `main` 或修改全局安全策略。

工作分支：`codex/fix-windows-node-unicode-fs`。本轮开始于 `1373fa7`；本地 ZIP 来自本轮工作区构建，不是已发布 Alpha.27 的新增下载资产。

## 已完成并验证

| 项目 | 实际结果 | 保留证据文件 |
| --- | --- | --- |
| 检查点收口 | 三个 PowerShell 脚本解析、打包脚本 Bash 语法、Web typecheck 均退出 0 | `checkpoint-source-gates.log`、`checkpoint-web-typecheck.log` |
| Rust 格式化、严格 Clippy、全部 targets | 退出 0；49 个测试二进制/套件，551 passed、0 failed、0 ignored | `rust-final-quality-gates.log`、`rust-final-totals.json` |
| 当前固定源码的前端客户端 | 240 文件，3182 passed | `web-client-current.log` |
| 兼容基线、插件账本、发行事实 | 三项退出 0；基线识别两个固定 Core checkout 的发行 job | `python-baselines-fixed.log` |
| Node 前置检查回归 | 61 passed，1 条需旧 Node 24.11.1 的场景未在新 Node 上执行 | `node-prerequisite-tests.log` |
| Market 检查及测试 | 检查通过；1056 passed、3 skipped；未把符号链接能力不足说成已修复 | `market-check-tests.log` |
| MSVC Release | `--locked --release --target x86_64-pc-windows-msvc --jobs 1`，静态 CRT，退出 0 | `release-msvc-static.log` |
| PE 依赖 | 仅 Windows 系统 DLL，未发现外置 VC++ 运行库依赖 | `release-pe-dependencies.txt` |
| Windows ZIP | 实际打包、SHA-256、文件清单、无链接检查、中文空格路径解压、两个 cmd 启动器版本检查通过 | `package-windows-direct.log` |
| 实际包内 Web/市场 | 用包内 `tessivum.cmd` 启动，中文界面和第一方市场已安装页面可见；未设置源码资源环境变量 | `packaged-ui.json`、`windows-packaged-market.png` |
| 真实 Quick Tunnel | 已校验的 cloudflared 2026.8.3 Windows exe；公网 HTTPS 配对、中文 Web、设备撤销通过 | `remote-real-tunnel.json`、`windows-remote-paired.png`、`windows-remote-revoked.png` |
| Agent Mode Browser 回归 | 4 passed；用 workspaceId 区分会话，不比较两种 Windows 路径字符串 | `browser-mode-selection-fixed.log` |
| Browser 路径修复 | lifecycle、market、permission context、produced files、scaffold、startup、workspace management 七个文件定向执行全部通过 | `browser-path-checkpoint.log` |
| 图片恢复及反馈 | 图片实际上传/模型选择/恢复 1 passed；反馈持久化与撤销 2 passed | `browser-live-checkpoint.log` |
| Remote Access Browser | 1 passed，51 个断言；修复真实重启后新 Windows Host 的回收 | `browser-remote-cleanup-fixed.log` |

原生进程回收、沙箱、插件别名目录修复已包含在上述 Rust 全量结果中。插件别名不再要求普通用户创建符号链接；沙箱清理等待已确认归属的进程后代。独立安全审查未发现这些生产改动的阻断问题。

## 未解决问题登记

### WIN-CHECKPOINT-01：Windows 非 UTC 命名时区调度

- **状态：未解决，阻断完整 Browser 验收。**
- 入口：`web/tests/schedule-after.e2e.ts` 的 `schedule-at preserves browser-local time through reload and dispatches once`。
- 表现：等待调度创建/执行条件超时。
- 证据：`src/schedule.rs` 的时区读取仍依赖 `/usr/share/zoneinfo/{name}`；普通原生 Windows 没有该数据库路径。
- 保留 `Asia/Shanghai` 场景，未改成仅验证 UTC。后续应解决原生时区解析/数据来源，保持夏令时、歧义及非法输入约束，而不是修改测试输入回避。
- 复现：在 `web` 执行 `bun test ./tests/schedule-after.e2e.ts --timeout 120000`。

### WIN-CHECKPOINT-02：跨 Host 共享设置夹具需要文件符号链接

- **状态：未解决，阻断完整 Browser 验收。**
- 入口：`web/tests/settings-chrome.e2e.ts` 的共享设置启动逻辑。
- 表现：普通用户且 Developer Mode 关闭时，`symlink(settings.yaml, settings.yaml)` 返回 `EPERM`。
- 不能直接换成复制或硬链接：设置保存采用原子替换，不能保证另一 Host 持续看到更新。整个数据目录 junction 又会共享工作区锁。
- 后续需要真实、非特权的独立设置路径配置方式；两个 Host 仍须保有独立的数据目录和锁。不提升权限，不跳过该场景。
- 复现：在 `web` 执行 `bun test ./tests/settings-chrome.e2e.ts --timeout 120000`。

### WIN-CHECKPOINT-03：发行 smoke 的无 Node PTC 环境仍未就绪

- **状态：未解决，阻断 Windows 发行流水线。**
- 命令：`pwsh -NoProfile -File scripts/smoke_windows_release.ps1 -ArchivePath <zip> -Version 0.1.0-alpha.27 -RepositoryRoot <repo> -LogDirectory <logs>`。
- 最后一次退出 1。已先在正常 PATH 下准备市场，再启动受限 PATH 的 PTC；仍报 `INVALID_HOST_CONFIG: invalid legacy Node profile: host program "bun" is not executable on PATH`。
- 日志：`windows-release-smoke-final.log` 与 `package-smoke-final/ptc.stderr.log`。
- 无 Node 的实际 `run_code` 执行验收尚未通过；后续梦境皮肤/Legacy 发行 smoke 阶段不能据此宣称已通过。已补充真实工具结果断言，未用预录助手文本当作工具成功证据。
- 后续检查受限 PATH 中 Bun 的实际可执行解析和环境构造；保留无 Node 的真实执行要求。

### WIN-CHECKPOINT-04：安装器故障边界脚本严格模式错误

- **状态：未解决，阻断安装器完整验收及发行流水线。**
- 命令：`pwsh -NoProfile -File scripts/test_install.ps1 -ArchivePath <zip>`；需要相邻 `.sha256`。
- 最后一次退出 1：`The property 'Length' cannot be found on this object. Verify that the property exists.`
- 日志：`windows-installer-final.log`。当前输出未保留精确脚本栈，不能将异常定位猜测当成已确认根因。
- 已修复安装器解析、提交前/后清理边界、回滚失败时保留恢复资料、空目录重复卸载；但完整故障注入尚未通过，**不建议用于正式安装/升级**。
- 后续应先保留错误栈，检查测试脚本中空内容/集合的 PowerShell 展开与 `Length` 使用，再执行完整真实 ZIP 的安装、回滚、卸载及用户数据保护场景。

完整 Browser 原始运行 `browser-portable-final.log` 退出 1。上表列出的定向修复均有后续通过记录，但没有声称重新获得整套绿色结果。所有未通过门槛仍然保留，Windows 发布 job 不应绕过这些门槛上传正式资产。

## 本地产物与环境

- ZIP：`dist/tessivum-0.1.0-alpha.27-x86_64-pc-windows-msvc.zip`，26,872,971 字节。
- SHA-256：`500b101d7d5e42236a78f7263a1a481a2d1d26cfe27e6916ed2ca96510b7e4b7`。
- ZIP、依赖 checkout、用户状态、日志和截图不随代码提交上传。完整本地证据位于工作目录外的 `windows-delivery-20260916`；日志命令元数据保存在同名 `.json` 中。
- Windows 11 x64、普通用户、Developer Mode 关闭；Rust/Cargo 1.94.0、Node 24.20.0、Bun 1.4.0、pnpm 11.7.0、PowerShell 7.6.4。
- 主源码使用已有全局 worktree；固定 DeepSeek 依赖、Cargo 输出及包验收使用中文/空格路径。本次不是新建完整源码 checkout 后一次无失败跑完全部验收组，不能替代那份严格验收证据。
- 原始失败目录未清除。依赖网络使用命令级覆盖失效代理，没有改全局 Git/代理配置；本地市场 tarball 因 Bun 的 Unicode 输出路径问题改用同内容的 `npm pack --ignore-scripts`。
- 没有发布 Windows ARM64、安装第三方运行时到用户 PATH、修改 Machine PATH、修改用户真实配置或声明 hosted Windows CI 已执行。

## 最终资源清理

全部子代理已回收，所有受管 Web 服务和验收浏览器已关闭。最终检查：3000/3001/3002 无监听，`tessivum.exe` 与 cloudflared 残留为 0，本次证据目录对应的 Chrome 进程为 0。记录见 `checkpoint-cleanup.json`。先前退出码 130 是主动 Ctrl+C；受管停止产生的退出码 1 不记作通过测试的退出码。
