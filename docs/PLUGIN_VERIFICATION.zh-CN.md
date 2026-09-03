# 社区插件兼容验证

Tessivum 不托管插件代码，也不维护第二套 Registry。插件发现与描述来自 [awesome-dsh-plugin](https://github.com/awesome-dsh-plugin/awesome-dsh-plugin)，代码由 npm 或不可变 GitHub Release 承载。Tessivum 只在 `plugins/market/compatibility.json` 保存精确版本的兼容证据。

## 进入目录

先向 [awesome-dsh-plugin](https://github.com/awesome-dsh-plugin/awesome-dsh-plugin/blob/main/contributing.md) 提交独立 YAML 条目。合并后，Market 会把该条目标记为 **DSH 社区 · 未验证**。

## 申请“Tessivum 已验证”

社区条目上线后，提交[插件验证申请](../.github/ISSUE_TEMPLATE/plugin-verification.yml)，并固定：

- 社区 identity 与源码仓库；
- npm 精确版本、Git commit 或不可变 Release archive；
- 许可证和包完整性；
- 目标 Profile 与 Native/WASM/Legacy Node/Browser runtime；
- `dsh.bundle`、`dsh.client` 和所需 service/capability；
- 最低 Tessivum 版本及待验证功能。

无密钥验证工作流检查来源/包身份、Profile preflight、精确安装、Host 与真实 Browser 启动、一个声明功能、更新、卸载、失败回滚、console/HTTP 错误和子进程残留。安装脚本默认禁止；只有单独审查后才能进入受限运行。

## 状态

- **Tessivum 官方**：由 Tessivum 拥有并发布。
- **Tessivum 已验证 · VERSION**：该社区精确版本通过了已记录矩阵。
- **DSH 社区 · 未验证**：已被上游目录收录，但当前没有精确版本证据。
- **Tessivum 验证已撤销 · VERSION**：原证据已撤回；ledger 必须记录原因。

验证是兼容证据，不是安全审计或背书。Legacy Node 与 Browser 插件是以用户权限运行的受信任第三方代码。

新版本不会继承旧版本的验证。Market 默认安装已验证的精确版本；更新到其他版本后，界面会改为 **未验证**，直到新证据合并。撤销不会静默卸载用户已有插件，但 Market 会明确显示撤销状态，后续安装或更新仍按未验证处理。

## 复现

```bash
python3 scripts/check_plugin_verification.py --network
VERIFY_PLUGIN=1 SAMPLES=1 ./benchmarks/run-linux-container.sh
```

第一条命令把 ledger 与社区 snapshot、npm metadata、repository、license、下载 tarball 完整性及已提交的生命周期工件逐项核对。第二条命令运行无密钥 Linux 精确安装、Host/Chromium、更新、卸载与失败回滚链路。已提交的 raw result 位于 `plugins/market/evidence/`；可读结果见[验证报告](PLUGIN_VERIFICATION_REPORT.md)。
