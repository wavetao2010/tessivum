# Windows 原生修复验收报告：2026-09-16

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
