# DeepSeek Web E2E 移植清单

基线：DeepSeek Harness `0.1.0-rc.5`，提交 `47f943859bef60e4160492346772ded9b24f765a`，目录 `apps/web/tests`。

## 规则

- `[ ]`：尚未在 Tessivum + 源 Web + 真实 Chromium 上通过；`[x]`：对应移植 spec 在普通模式通过。
- 每行保留一个可运行 spec；可以替换 Host scaffold，但不能删减用户可见断言。
- 所有行共同要求：从源 Web boot graph 启动、真实 HTTP/WebSocket wire、真实 Chromium、`pageerror=[]`、受监控 `console.warn/error=[]`。
- fixture 录制只生成确定性 LLM 输入；普通断言模式必须消费完 fixture，并校验 committed golden inventory。
- 不因 Tessivum 暂无能力而 `skip`、伪造 RPC 成功或改成浅层截图测试。缺能力先实现契约，再勾选。
- 状态以本文件为准；现有 `tessivum/web` build/test 通过不代表任何上游 E2E 已移植。
- Legacy Node 不注入 `agentCore`、`llm`、`systemPrompt`、`sessionStore` 或 `toolRuntime`；依赖这些模块的插件必须显式失败，不能用成功 no-op、空结果或伪服务让 Web spec 通过。

## Gate 缩写

- `SRC`：上游源码 Web、Web profile 的 33 个显式 Browser roster 行、5 个辅助 `dsh.client` 行（共 38）、package-name graph 与 scoped package bundle route；该 gate 已具备运行实现，但单个场景仍需与其余 gate 一起通过后才能勾选。
- `WIRE`：52 个 Core RPC、24 个 Typert Remote、两个下行 WebSocket 与 response wire。
- `LLM`：上游 `GenerateOptions` / `StreamChunk`、replay、retry、cancel 语义。
- `NODE`：`cordis.node/v1` 插件桥。
- `PKG:<name>`：对应上游 Client/Host plugin 还未组合或实现。
- `HOST:<name>`：对应 Host domain 能力缺失。

- `access-confirmation.e2e.ts`、`agent-preset-authoring.e2e.ts` 与 `agent-preset-selection.e2e.ts` 已达到完整 gate；其余条目不能沿用旧 bundle 的局部可见行为提前勾选。

## 逐文件清单（69/69）

| 状态 | # | 上游 spec | 必须保留的验收行为 | 主要 gate |
|---|---:|---|---|---|
| [x] | 1 | `access-confirmation.e2e.ts` | Full Access 必须先确认风险；确认后 composer 才能启用。 | `HOST:permission`, `WIRE` |
| [x] | 2 | `agent-preset-authoring.e2e.ts` | preset roster、只读查看、整份复制、删除/损坏清理和 creator session 全链路。 | `HOST:agentPreset`, `WIRE` |
| [x] | 3 | `agent-preset-selection.e2e.ts` | hero 选择、preset 说明、staged pick、slash catalog 重读及冷恢复标签正确。 | `HOST:agentPreset`, `WIRE` |
| [x] | 4 | `approval-composer.e2e.ts` | 长命令审批卡限高且操作可达；批准后命令真实执行并 settle。 | `HOST:approval+toolRuntime`, `WIRE` |
| [x] | 5 | `background-job-list.e2e.ts` | running job 无刷新出现；取消 settle 后打开列表原位更新。 | `HOST:jobs`, `WIRE` |
| [x] | 6 | `bash-abort-row.e2e.ts` | cancelled Bash 行可展开到完整命令和错误。 | `HOST:toolRuntime`, `LLM` |
| [x] | 7 | `chat-continuous-conversation.e2e.ts` | 12 个连续 turn 与 tool 行保持在同一 live session，顺序和身份稳定。 | `LLM`, `WIRE` |
| [x] | 8 | `chat-long-interactions.e2e.ts` | 长异构历史行身份/顺序、tool 展开、复制、分支和子会话继续对话正确。 | `HOST:session`, `LLM` |
| [x] | 9 | `chat-scroll-contract.e2e.ts` | history prepend+stream、tool disclosure、tab/session 恢复、composer resize、键盘分页和 touch fling 都维持正确锚点/跟随权。 | `PKG:client-ui-conversation` |
| [x] | 10 | `code-mode-round.e2e.ts` | `run_code` durable call 带完整 sub-dispatch；嵌套行常驻且点击不误开 details。 | `HOST:toolRuntime`, `LLM` |
| [x] | 11 | `cold-blank-session.e2e.ts` | 已验证的冷 blank session 不出现在 sidebar。 | `HOST:session`, `WIRE` |
| [x] | 12 | `composer-draft-scroll.e2e.ts` | 长草稿三层同宽同 scroll；wheel、编辑、paste、末尾换行和 caret reveal 几何正确。 | `PKG:client-ui-input-trigger` |
| [x] | 13 | `composer-tab-geometry.e2e.ts` | Chat/Trajectory 切换时 input card 在宽/窄 viewport 都不位移，mutation control 能证明断言非空。 | `PKG:client-ui-conversation` |
| [x] | 14 | `conversation-column-overflow.e2e.ts` | glow 越界时 conversation column 仍只纵向滚动；解除 `overflow-x` 的 control 必须复现横向滚动。 | `PKG:client-ui-conversation` |
| [x] | 15 | `cordis-tool-round.e2e.ts` | 完整 Cordis lifecycle 写入 durable log，并渲染本地化 owned cards。 | `PKG:client-ui-cordis`, `NODE` |
| [x] | 16 | `declared-reasoning.e2e.ts` | composer 只提供 provider 声明的 reasoning levels，并记录所选值。 | `HOST:llm`, `WIRE` |
| [x] | 17 | `default-model.e2e.ts` | composer 切换成为后续 session 默认，不改已记录 session；route 消失后变 inert。 | `HOST:settings+llm`, `WIRE` |
| [x] | 18 | `details-session-lifecycle.e2e.ts` | details 初始/reload 关闭，并在 session ownership 变化后保持关闭。 | `PKG:client-ui-trajectory` |
| [x] | 19 | `feedback-command.e2e.ts` | `/feedback` 记录反馈，并以 session id 和 sharing 状态渲染 acknowledgement。 | `PKG:commands`, `WIRE` |
| [x] | 20 | `goal-bar.e2e.ts` | 一个 active goal 正确显示，clear 后收敛且不暴露 stale error。 | `HOST:goal`, `WIRE` |
| [x] | 21 | `goal-command-presentation.e2e.ts` | fresh session 的 `/goal` 显示 bare input/result、无模型 turn；reload 从 durable lifecycle 重建同一内容。 | `HOST:goal+commands` |
| [x] | 22 | `goal-multi-turn-actions.e2e.ts` | 两轮 Goal 的每个 completed turn tail 各保留且只保留一个 assistant action。 | `HOST:goal+toolRuntime`, `LLM` |
| [x] | 23 | `hmr-live.e2e.ts` | 修改真实 Client plugin 源文件后热更新，不刷新页面。 | `PKG:client-hmr`, `SRC` |
| [x] | 24 | `lifecycle-chrome.e2e.ts` | command menu、active Plan、hero 首发、workspace/session materialize、reload 和暗色级联完整。 | `HOST:workspace+session+settings`, `WIRE` |
| [x] | 25 | `live-interactions.e2e.ts` | hung stream 可取消；AUTH 脱敏且不重试；SERVER 重试后完成；trajectory terminal marker 正确。 | `LLM`, `WIRE` |
| [x] | 26 | `markdown-cjk-strong.e2e.ts` | CJK 相邻、标点终止的 strong span 正确渲染。 | `PKG:client-ui-conversation` |
| [x] | 27 | `markdown-images.e2e.ts` | 只加载指定远程 Markdown image，并匹配 conversation golden。 | `PKG:client-ui-conversation` |
| [x] | 28 | `markdown-inline-code-links.e2e.ts` | inline code 中完整 HTTP URL 可打开，其他 code 保持 inert。 | `PKG:client-ui-conversation` |
| [x] | 29 | `math-rendering.e2e.ts` | settled Markdown math 无 KaTeX error 且匹配 golden。 | `PKG:client-ui-conversation` |
| [x] | 30 | `message-actions.e2e.ts` | branch 只在 completed transcript tail 启用；IconActions/clocks、消息分支和 session-row 分支正确。 | `HOST:session`, `PKG:client-ui-conversation` |
| [x] | 31 | `message-feedback.e2e.ts` | rating/note 跨 reload 持久化，随后可撤回。 | `PKG:message-feedback`, `WIRE` |
| [x] | 32 | `models-settings.e2e.ts` | dormant/custom provider 的 key 校验、native auth、保存/merge patch、declare/edit/delete 全闭环。 | `HOST:settings+credentials+llm`, `WIRE` |
| [x] | 33 | `navigation-panes.e2e.ts` | 冷 session 内容搜索、Trajectory/inspector、session export、timeline drag 和 terminal card 正确。 | `HOST:session.search`, `PKG:client-ui-trajectory` |
| [x] | 34 | `onboarding-deepseek-config.e2e.ts` | key write-only 配置即时生效；configured reload 不闪 takeover；任意 DeepSeek model 可配置且删除选择后可恢复。 | `HOST:credentials+llm`, `WIRE` |
| [x] | 35 | `onboarding-usable-provider.e2e.ts` | setup card cancel 不丢 add card；其他可用 provider 配好后停止 DeepSeek onboarding。 | `HOST:credentials+llm`, `WIRE` |
| [x] | 36 | `permission-policy-context.e2e.ts` | read-only/full-access/workspace-write 经 GUI 切换，并在对应模型行为前进入 current system context。 | `HOST:permission`, `LLM` |
| [x] | 37 | `plan-review.e2e.ts` | plan decision card 展示并通过真实 response wire 批准，后续 turn 正常。 | `HOST:planning`, `WIRE` |
| [x] | 38 | `plugin-config.e2e.ts` | 每个 exposed Host namespace 一张卡；save/discard/invalid/reset 精确更新 document。 | `PKG:plugin-inventory`, `HOST:settings` |
| [x] | 39 | `produced-file-mentions.e2e.ts` | unique inline-code 文件 mention 可打开；ambiguous/unknown mention inert。 | `HOST:filesystem`, `PKG:client-ui-tool` |
| [x] | 40 | `produced-files.e2e.ts` | 十文件窄 summary 单行显示 `+8` 和 folder action。 | `HOST:filesystem`, `PKG:client-ui-tool` |
| [x] | 41 | `pwa-manifest.e2e.ts` | built Web 含 install metadata；favicon 在 dark color scheme 切换 light mark。 | `SRC` |
| [x] | 42 | `pwsh-terminal.e2e.ts` | seeded pwsh call 使用 bash terminal-card layout，并解析 exit pill。 | `HOST:toolRuntime`, `PKG:client-ui-tool` |
| [x] | 43 | `question-composer.e2e.ts` | question 驻留 composer、可回答并完成，答案写入 log。 | `HOST:question`, `WIRE` |
| [x] | 44 | `queue-actions.e2e.ts` | queue 精确 occurrence 编辑/删除、stop 后 FIFO 保留；Todo→Goal→Queue 响应式顺序正确。 | `HOST:session.queue`, `WIRE` |
| [x] | 45 | `remote-welcome.e2e.ts` | remote welcome 令 root inert；process-local advance 后 reload 再次展示。 | `PKG:client-ui-welcome` |
| [x] | 46 | `replay-round-trip.e2e.ts` | 真实组装完成 recorded round；request header/Web URL、Markdown/tool/reasoning fold/composer restore 正确。 | `LLM`, `WIRE` |
| [x] | 47 | `scaffold-hermetic.e2e.ts` | replay skill discovery 与所有 ambient Host roots 隔离。 | `HOST:skill`, `LLM` |
| [x] | 48 | `schedule-after.e2e.ts` | After/Every/At reminder 作为普通 assistant follow-up；Every 只批最新 overdue occurrence，At 使用请求本地浏览器上下文。 | `HOST:schedule`, `LLM` |
| [x] | 49 | `seeded-history.e2e.ts` | cold log 提供 projection baseline、sidebar/history、context disclosure、tool rows、compaction、command/feedback rows和短 context 几何。 | `HOST:session+projection`, `WIRE` |
| [x] | 50 | `settings-chrome.e2e.ts` | settings modal 全关闭路径；默认 Permission、boot theme、appearance、busy Enter 和 locale 跨 reload/port 持久化。 | `HOST:settings`, `WIRE` |
| [x] | 51 | `shipped-composition.e2e.ts` | shipped Web catalog、文件引用指导、confined 默认和 preset→background-job registry 真实组装正确。 | `SRC`, `HOST:composition` |
| [x] | 52 | `sidebar-scrollbar.e2e.ts` | overflow gutter、hover/linger thumb、非 overflow inset、双 palette WebKit thumb 和 geometry golden 正确。 | `PKG:client-ui-sidebar` |
| [x] | 53 | `sidebar-subagent-activity.e2e.ts` | running descendant activity 固定显示在可见 idle owner row。 | `HOST:subagent`, `WIRE` |
| [x] | 54 | `skill-invocation-policy.e2e.ts` | slash menu 呈现所有 user-invocable skill，并标记 user-only entry。 | `HOST:skill`, `WIRE` |
| [x] | 55 | `skill-tool-row.e2e.ts` | dedicated Skill row 可展开到精确 recorded instructions。 | `HOST:skill+toolRuntime` |
| [x] | 56 | `skill-user-invoke.e2e.ts` | `/name args` 生成 gesture bubble、injection row 和 replayed answer。 | `HOST:skill`, `LLM` |
| [x] | 57 | `smoke-real.e2e.ts` | CLI 默认 loopback、workspace/system context、partial retry、code mode；有 key 时真实首轮、tabs、bash、resize、dark、reload 全程通过。 | `SRC`, `LLM`, `WIRE` |
| [x] | 58 | `startup-auto-selection.e2e.ts` | 首个 workspace session 出现/auto-select blank session 时复用 resident Hero/composer 节点且不中断画面。 | `HOST:workspace+session`, `WIRE` |
| [x] | 59 | `stats-paged-history.e2e.ts` | partial tail page 显示 whole-session stats，load older 后数值保持。 | `HOST:session.history`, `WIRE` |
| [x] | 60 | `steering.e2e.ts` | mid-turn steer durable/可见/被遵循；Cmd+Enter 与 swapped busy policy 正确；空草稿可一次 flush 全 queue。 | `HOST:session.queue`, `LLM` |
| [x] | 61 | `subagent-conversation.e2e.ts` | stale catalog 不丢 descendants；层级展开/冷打开/只读 one-shot/follow-up/fork/resume 都保持正确 ownership。 | `HOST:subagent`, `WIRE` |
| [x] | 62 | `subagent-interrupt-ui.e2e.ts` | parent offline 时仍可从 composer interrupt live child；恢复后 follow-up 以 FIFO 继续。 | `HOST:subagent`, `WIRE` |
| [x] | 63 | `subagent-interrupt.e2e.ts` | 真实 composition 上 interrupt 将 queued follow-up 停放，waking send 后 FIFO resume。 | `HOST:subagent`, `WIRE` |
| [x] | 64 | `trajectory-virtualization.e2e.ts` | tail-paged Trajectory prepend 保持 row identity，并能到达 bounded virtual range。 | `HOST:session.history`, `PKG:client-ui-trajectory` |
| [x] | 65 | `turn-tail-actions.e2e.ts` | assistant IconActions 在 running turn 隐藏，只在 `turn/end` 后出现。 | `PKG:client-ui-conversation`, `WIRE` |
| [x] | 66 | `vite-entry.e2e.ts` | package dev alias 与 standalone Vite server 都拒绝启动，并指向完整 Host 启动方式。 | `SRC` |
| [x] | 67 | `web-search-round.e2e.ts` | shipped provider 真实调用、结构化结果持久化并限长；search card/source scroll/marker room 正确。 | `HOST:webSearch+toolRuntime`, `LLM` |
| [x] | 68 | `workflow-run.e2e.ts` | live workflow member 可打开 child；settle 后记录与 tool row 共存，reload 从 history 重建。 | `HOST:workflow+subagent`, `WIRE` |
| [x] | 69 | `workspace-management.e2e.ts` | create/rename/delete/reuse、flat view、directory browser、hover/menu、archive 和同 basename 不同路径全闭环。 | `HOST:workspace`, `WIRE` |

## 推荐执行顺序

这是依赖顺序，不是缩减范围：

1. `SRC`：源 Web、完整 boot graph、33 个 Client plugin bundle 可加载。
2. `WIRE`：HTTP RPC、两个 WebSocket、Remote contributions、11 个转发事件。
3. workspace/session/settings/credentials/LLM 最小真实链路；先跑 `lifecycle-chrome.e2e.ts`、`chat-continuous-conversation.e2e.ts`。
4. queue/approval/question/tool/goal/subagent 等 Host domains。
5. UI-only markdown/layout/shortcut/geometry specs。
6. 最后运行全部 69 个 spec 和 fixture inventory；只有此时才能宣称 DeepSeek Web 兼容完成。
