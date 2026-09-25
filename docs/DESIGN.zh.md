# OCS Pi Extension（原「网页面板 / AI 助手」线）— 交接与状态

> 2026-09-16 Phase 2 已交付（提交 `5f946787`）。本文件继续作为唯一入口。

## 1. 定位与命名

- 正式名：**OCS Pi Extension**（宿主内建面板，标题「Pi 助手」，命令 `PI`，别名 `OCSPI` / `AI` / `AICHAT`）。
- **它是宿主内建能力，不是插件**：宿主插件 API 没有「注册 UI 面板」的能力，而 iced 控件无法跨进程传递，
  所以面板内容必须由宿主自己用 iced 画（`src/ui/pi_panel.rs`）。
- 通用「网页面板」方案已废弃删除（见 Phase 1 交接史，提交 `a4f122ec`）。

## 2. 现状（Phase 2 完成，已实机验证）

| 提交 | 内容 |
|---|---|
| `a4f122ec`~`01281249` | Phase 1：原生停靠面板骨架、宽度策略、Fixed(width) 真因修复、改名 Pi |
| `5f946787` | **Phase 2**：聊天面板完整交付（见下） |

Phase 2 功能清单（用户已定案：工具/思考默认折叠、流式中发送排队、跟随最近活跃+顶部会话切换下拉、不接审批 UI）：

- **数据层 `src/pi.rs`**：worker（会话列表/命令）+ reader（SSE 解析）双线程；
  事件全覆盖：`message_update`（text/thinking 流式 delta、toolcall）、`message_start|end`、
  `tool_execution_*`、`agent_start|end`、`queue_update`、`startup_error`；
  **chunked 响应解码**（Next.js 全部走 Transfer-Encoding: chunked，不 decode 连 sessions 都解析不了）；
  POST 用 **`{"type":"prompt","message":…}`**（不是 `{"prompt":…}`，会被 `prompt_rejected` 拒绝），
  流式中自动加 `streamingBehavior:"followUp"` 走服务端排队；
  会话列表 + `Watch(id)` 切换 + session `.jsonl` 尾部回填历史。18 个单测。
- **UI `src/ui/pi_panel.rs`**：原生头部（图钉/关闭/DockGrab）+ 会话下拉 + 状态行（重连）+
  滚动消息列表（用户气泡/助手文本/思考折叠/工具折叠带「完成绿/出错红/运行中紫」徽标）+
  底部多行输入框（`text_editor` + `.key_binding` 拦截 Enter：无修饰发送、Shift+Enter 换行）+
  排队/流式提示。事件折叠状态机 `PiPanelState::apply`：乐观气泡 + 回声去重、
  流式原地追加（按 contentIndex 分块）、工具按 call-id 幂等合并、
  条目 id 用内容 FNV 哈希（重连回填后展开状态保持）。9 个单测。
- **接线**：`Message::Pi(PiMsg)` → `src/app/update/pi.rs`；10Hz `PiMsg::Poll` 订阅（仅面板可见时）；
  面板开/关（PI 命令、DockMsg::Close）起停 worker。

已实机验证（OCS 运行中 + pi-web 本地服务）：跟随最近活跃会话、历史回填（消息/思考/工具折叠行）、
实时工具流（新 toolcall 实时流入面板）、composer 输入+Enter 排队（「已排队 1 条」）、
乐观气泡 + 输入清空、折叠行渲染（▸/▾ + 徽标）。

## 3. Phase 2 中踩到的坑（血泪新增）

- **Next.js 的 HTTP 响应全部是 chunked**：手写 HTTP 客户端必须 de-chunk，否则 body 前是
  hex 长度行，`serde_json` 全部失败（症状：`pi-web 没有会话`）。
- **POST body 是 `{type:"prompt", message}`**：pi-web route 取 `body.type==="prompt"` 才接受；
  `{"prompt":…}` 返回 500 + `prompt_rejected`。排队加 `streamingBehavior:"followUp"`（steer=打断改向）。
- **iced `text_editor` 的 Enter/Shift+Enter**：本版本有 `.key_binding(|KeyPress| -> Option<Binding>)`，
  拦截 `Key::Named(Named::Enter) && !modifiers.shift()` 返回 `Binding::Custom(发送)`，
  其余走 `Binding::from_key_press(kp)`。
- **滚动到底**：`iced::widget::operation::snap_to_end(widget::Id)` 现成的，别用相对 offset 凑。
- **pick_list 助手函数参数顺序**是 `(selected, options, to_string)`（与 `PickList::new` 相同，先 selected）。
- **消息列表自动滚底 + 实时流**会让 GUI 点击坐标漂移（行在截图与点击之间移动）；
  自动化点击验证要么切到安静会话，要么接受漂移靠单测兜底。
- **验收环境**：KWin Wayland 屏 2560×1600 物理 / **2048×1280 逻辑（缩放 1.25）**；
  uinput 相对位移被 libinput 加速（≈2×）不可控 → 用 **ABS 触摸设备**（`/tmp/ui-touch.py`，
  ABS 范围映射逻辑屏）精确点击；`wtype` 在 KWin 不可用（无虚拟键盘协议）；
  键盘输入也用 uinput（设备要 UI_SET_KEYBIT 全部字母，`goto` 后等 0.25s 设备 settle）。
  每次截图会弹 Spectacle 通知（挡面板右上），截图用 delay 0 或先点 × 关通知。
- **视觉验收用子代理量截图**（沿用 Phase 1 约定）。

## 4. 关键契约与坑（Phase 1 沿用）

- 面板 `view(width, …)` 必须 `.width(Length::Fixed(width))`（正确范例 `src/ui/properties.rs`）。
- dock 配置：`~/.config/OpenCADStudio/settings.json` 顶层 `dock` 键；**改配置先关 OCS**。
  当前：`pi: {width: 347.6, auto_collapse: false}`。
- 构建/重启：`cargo build --release`（≈1.5min）→ `pkill -x OpenCADStudio` → `bash /tmp/launch-ocs.sh 1178`。
  自动化接口：`mcporter call ocs.ocs_sessions` → `ocs.ocs_execute {request:{op:"run",cmd:"PI"},ocs_session_id,request_id}`（**request_id 在 request 对象里**）。
  欢迎页不渲染停靠栏，先 `op:"new"`。

## 5. pi-web 本地 API 契约（实测版，详见 wiki）

| 用途 | 请求 |
|---|---|
| 会话列表 | `GET /api/sessions` → `{"sessions":[{id,path,cwd,modified,messageCount,firstMessage}]}`（按 modified 降序） |
| 事件流 | `GET /api/agent/<id>/events` → SSE `data: {json}`（30s 心跳 `: \n\n`） |
| 发消息 | `POST /api/agent/<id>`，body `{"type":"prompt","message":"…"}`（排队加 `"streamingBehavior":"followUp"`） |

SSE 事件（wire 过滤 turn_start/turn_end、message_update 的 partial 已剥除）：
`connected{sessionId,isStreaming}`、`message_start|end{message{role,content[],toolCallId,toolName}}`、
`message_update{assistantMessageEvent{type:text_start|text_delta|text_end|thinking_*|toolcall_start|toolcall_end|error,
contentIndex,delta/content/toolCall{id,toolName}}}`、
`tool_execution_start|update|end{toolCallId,toolName,partialResult|result}`、
`agent_start|agent_end`、`agent_settled`、`queue_update{steering[],followUp[]}`、
`startup_error{errorMessage}`、`extension_ui_request`（暂未接）。
默认端点 `http://127.0.0.1:30141`（`OCS_PI_ENDPOINT` 覆盖）。
pi-web 源码：`agegr/pi-web`；事件投影逻辑 `lib/agent-event-wire.ts`、
`app/api/agent/[id]/events/route.ts`；客户端消费范例 `hooks/useAgentSession.ts`。

## 6. 文件地图

```
src/pi.rs                        pi-web 客户端：worker/reader、SSE 映射、dechunk、回填、排队（18 测）
src/ui/pi_panel.rs               面板：状态机 apply() + 全套 UI（9 测）
src/app/update/pi.rs             Message::Pi 处理器（Poll/Editor/Send/SessionPick/Toggle/Reconnect）
src/app/update/mod.rs            TogglePiPanel 开停 worker；mod pi;
src/app/update/dialog.rs         DockMsg::Close → stop_worker
src/app/view/mod.rs              10Hz PiPoll 订阅；expanded_panel Pi 分支
src/app/mod.rs                   Message::Pi(PiMsg)
src/ui/dock.rs                   PanelId::Pi 宽度策略（150 / 40% / 340 / 默认钉住）
src/app/document.rs              tab 字段 pi_panel
src/app/commands/display.rs      PI / OCSPI / AI / AICHAT 命令
```

## 7. Phase 2.5（2026-09-16 第二轮，提交 `2a365995`）

用户追加的 5 个功能，全部完成并实机验证：

1. **markdown 渲染**：助手消息（`PiEntryKind::Assistant { text, md }`）与流式文本
   （`StreamBlock::Text { md }`）用 `iced::widget::markdown` 渲染；
   `Content::parse` 静态解析、`Content::push_str` 流式增量解析；链接 → `Message::OpenUrl`；
   `Settings::with_text_size(12, theme)` 需要 Theme 提前传入（面板视图签名带 `&Theme`，
   来自 `OpenCADStudio::active_theme`）。
2. **粘贴图片（替代内置截图）**：用户定案 —— 截图走系统工具（Spectacle）复制到剪贴板，
   面板只负责粘贴。composer 的 `key_binding` 里 Ctrl+V（focused 时）→ `PiMsg::Paste` →
   `iced::clipboard::read_image()` 优先；成功则 RGBA→PNG→base64 存 `pending_image`
   （长边缩至 1600、PNG >8 MB 拒绝），输入框上方显示 chip（大小/尺寸/✕）；
   失败回退 `iced::clipboard::read_text()` → `Edit::Paste(text)` 插入文本。
   发送时 POST `images:[{type:"image",data(<raw base64>),mimeType}]`。
   **注意**：pi-web 侧校验（`lib/image-attachments.ts`）要求裸 base64、≤10 MB、≤10 张，
   校验失败会整条 `prompt_rejected`。空文本+纯图发送合法（optimistic 气泡显示「📷 截图」）。
3. **选中标签**：输入框上方一行「◉ 圆弧（1）」；规则 = 单一类型→`t!(类型名)（n）`，
   混合类型→`t!("All")（n）`，与特性面板 `build_selection_groups` 同源翻译；
   在 `PiMsg::Poll` 里用 `scene.selection_fingerprint()`（缓存哈希）判断变化才重算
   （`selection_label` 存 per-tab 状态，避免视图借用临时值）。
4. **工具/思考折叠行整行宽**：`toggle_row` 的 button 加 `.width(Length::Fill)`。
5. **模型下拉**：输入框下方 `pick_list`；worker 连接后 `GET /api/models`（`modelList`
   81 项）+ `GET /api/agent/<id>`（`state.model`）→ `Event::Models`；选择 → `Command::SetModel`
   → POST `{type:"set_model",provider,modelId}` → `Event::ModelSet`。
   （实测用户已用它在面板里把模型切到 DeepSeek V4 Pro (New)。）

### 本轮验收注意
- GUI 自动化：uinput 触摸设备（`/tmp/ui-touch.py`）点击/输入；**键盘组合键（Ctrl+V）需要
  设备注册 CTRL(29) 的 KEYBIT**，否则修饰键被内核丢弃、V 变成裸字符（或被丢弃）。
- 截图/通知：Spectacle 通知会反复遮挡面板右上；截图尽量小裁剪、间隔拉长
  （用户反馈「一次上传太多图报 400」）。
- 待观察：带图消息在回合结束交付后，session `.jsonl` 的 user message 应含 image part
  （本轮已验证 POST 通过 pi-web 校验并进入 followUp 队列）。

## 8. Phase 2.6（A 期四功能，提交 `29175b9f`）

用户问的五问 → 先做 A 期四项（B 期 RPC 后端见第 10 节）：

1. **思考强度下拉**：worker 在 `/api/models` 里解析 `thinkingLevels: {"provider:id":[…]}`，
   并在 `GET /api/agent/<id>` 取 `state.thinkingLevel`；模型下拉旁显示「思考:max」，
   选择 → POST `{type:"set_thinking_level", level}` → `Event::ThinkingSet`。
2. **新建会话 + 目录浏览**：会话行「＋」按钮 → `open_browse()` → `GET /api/cwd/browse?path=`
   （↑ 上级 / 8 个子目录 / 在此新建会话 / 取消）→ POST `/api/agent/new`
   `{cwd, type:"ensure_session"}` → 返回 `sessionId` → worker 设 `watch_target` 并重连跟随。
   注意：**切换目录 = 用新 cwd 新建会话**（会话的项目目录创建时固定）。
3. **命令补全**：worker 连接后 `{type:"get_commands"}` → `Event::Commands`；
   composer 行首 `/` 弹层（名称 + `[source]` + 描述，子序列模糊过滤，最多 8 行可见）；
   `key_binding` 在弹层打开时接管 ↑/↓/Enter/Tab/Esc；Enter 填入 `/{name} `（不发送）。
4. **@ 文件引用**：`@` 触发文件补全（`/api/file-index?cwd=` 一次拉 5000 条缓存进状态，
   Rust 端子序列模糊匹配）；**发送时展开**：`@path`/`@"带 空格"` → 按 pi CLI 语义
   拼 `<file name="绝对路径">\n内容\n</file>` 块（图片走 image 附件通道；文本 ≤512KB、
   图片 ≤8MB、最多 12 个；失败发 Notice）。

实机验证：思考下拉显示「思考:max」；＋ 打开目录浏览器（默认目录 + 子目录列表）；
`/` 列出 extension 命令（`/websearch` `/wiki-trajectories` `/wiki-model`）并可 Enter 填入；
`@` 列出文件匹配并可 Enter 插入路径。

## 9. Phase 2.7（2026-09-16 第四轮，提交 `1b8bff69`）

用户追加四项 + 一个 UI 修正，全部完成：

1. **表格横向滚动**：iced 内置 markdown 表格在窄面板会把单元格挤成单字列
   （`Row.cells` 是私有字段，没法只覆盖 Viewer::table）。改法：数据层
   `AssistantBody { text, md, table_split }` —— 文本含管道表格时用 `split_tables()`
   切成「markdown 段 / 表格段」，表格段用 `150px` 固定列宽 + 水平滚动渲染；
   不含表格的消息走原生快速路径；**流式期间走快速路径，定稿后表格化**（避免每 delta 重解析）。
   2 个单测（分段解析、仅在含表格时分段）。
2. **自由文本审批**：`input`/`editor` 类 extension UI 请求在警示行内渲染 `text_editor`
   （editor 高 84px / input 34px），`prefill` 自动回填，`确定`(UiSubmit) / `取消`；
   `UiEdit` 转发编辑器动作。select/confirm 保持按钮式。
3. **压缩按钮**：模型行右端「压缩」，流式中禁用（`is_streaming()` 时 `on_press_maybe(None)`）；
   `Command::Compact` → web `POST {type:"compact"}` / RPC `{"type":"compact"}`；
   成败分别由 `compaction_*` 事件（Notice）与 `SendFailed` 呈现。
4. **用量统计**：`Command::FetchStats` → `get_session_stats` → `Event::Stats(SessionStats)`；
   展示 `↑输入 ↓输出 · 缓存 读/写 · 上下文 百分比（tokens/window） · $成本`；
   触发时机 = 连接就绪（`Status::Ready`）+ 每轮流式结束后（`Streaming(false)` 且有转换）。
   `contextUsage.percent` 为 null 时用 `tokens*100/window` 自算。
5. **滚动条让位**：iced 的滚动条是**浮层**（无预留空间模式）→ transcript 内容加 12px 右
   padding、表格容器加 12px 底 padding，滚动条落在 padding 内不再遮挡内容
   （像素采样验证：行背景右边界距面板右缘 ~20px）。

**踩坑记录**：
- `markdown::Row.cells` / `Column` 的字段私有 → 无法在自定义 `Viewer` 里改列宽；
  只能走文本分段 + 自建 `table` 组件。
- 表格/分段的 `markdown::Content` **不是 `Clone`** → 分段必须在数据层构建并随条目存储
  （渲染时构建会撞 E0515 借用局部变量）。
- 面板 GUI 自动化的两点教训：会话下拉菜单项坐标随列表长度漂移（先截图定位再点）；
  合盖提交时 `git add -A` 会误扫**其他并行会话**在仓库里的未提交改动 —— 提交前用
  `git status` 核对，误扫后用 `git reset --soft HEAD~1` + `git restore --staged` 拆开。

## 10. 可选后续（Phase 3 候选，未做）

1. `extension_ui_request`（`setWidget`，如 `bash-bg` 后台任务小部件）→ 面板底部一行状态。
2. 审批交互（用户已定不接；若 pi 侧策略变化再议）。
3. 发送失败自动重试、历史回填条数可配置。
4. 会话下拉的人工点选（Watch/Replace 逻辑有单测覆盖）。

## 11. B 期已实施：pi RPC 后端（不依赖 pi-web）

**问题**：当前面板依赖 pi-web（本地 HTTP + SSE）。只有 pi（CLI）时应能工作。

**pi 自带两条路**（`docs/rpc.md` / `docs/sdk.md`，1578/1205 行）：
- `pi --mode rpc`：JSONL over stdin/stdout（一行一 JSON，LF 分隔；`\r\n` 容错）。
- Node SDK `AgentSession`（Rust 宿主不适用，只能走 RPC 子进程）。

**RPC 命令面（比 pi-web HTTP 更全）**：
`prompt`/`steer`/`follow_up`（含 images + streamingBehavior）、`abort`、`new_session`、
`switch_session{sessionPath}`、`fork`/`clone`/`get_fork_messages`、`get_state`/`get_messages`/
`get_entries`/`get_tree`/`get_session_stats`/`export_html`/`set_session_name`、
`set_model`/`cycle_model`/`get_available_models`、`set_thinking_level`/`cycle_thinking_level`/
`get_available_thinking_levels`、`get_commands`、`compact`/`set_auto_compaction`、
`set_auto_retry`/`abort_retry`、`bash`/`abort_bash`、`set_steering_mode`/`set_follow_up_mode`。
事件类型与 pi-web SSE 同源（message_start/update/end、tool_execution_*、agent_start/end/settled、
queue_update、compaction_*、auto_retry_*、extension_error），**现有 `sse_to_events` 映射可复用**。
另有 **Extension UI Requests**（select/confirm 交互）→ 可接审批弹窗（此前搁置项）。

**已实现（`src/pi_rpc.rs`，7 个单测）**：

- **模式选择**（`PiHandle::start_auto`）：`OCS_PI_MODE=auto|web|rpc`（默认 auto）+
  `OCS_PI_BIN`（默认 `pi`）+ `OCS_PI_CWD`（默认 `$HOME`）。auto = 150ms TCP 探测
  `OCS_PI_ENDPOINT`（默认 127.0.0.1:30141）可达 → web，否则 RPC 子进程。
- **RPC worker**：`pi --mode rpc` 子进程；单线程消息循环（stdout 解析线程 → 通道 →
  循环里分流：`type:"response"` 走握手/动作处理，其余按事件映射到同一个 `Event`，
  **完全复用 `pi::sse_to_events`**）。停止/重连时 kill 子进程。
- **握手**：`get_state`（sessionId/model/thinkingLevel/isStreaming → `Status::Ready` +
  `Models`）、`get_available_models`、`get_available_thinking_levels`、
  `get_commands`；会话列表由本地扫描 `~/.pi/agent/sessions/--<cwd 编码>--/*.jsonl`
  （slug = cwd 去首斜杠、`/`与`:`→`-`、两侧 `--`）得到（id=文件名 uuid，label=首条
  user 消息首行）。
- **命令映射**：Send→`prompt`（流式中自动 `streamingBehavior:"followUp"`，图片/`@` 展开同 web）、
  SetModel→`set_model`、SetThinking→`set_thinking_level`、Watch→`switch_session{sessionPath}`
  + `get_messages` 回填（`pi::messages_to_entries`）、NewSession→同 cwd 用 `new_session`，
  换目录则重启子进程（项目目录随进程）、Browse/FetchFiles→**本地 fs/git 实现**
  （`git ls-files` 或 BFS walk，跳过 vendor 目录，上限 5000）、UiRespond→`extension_ui_response`。
- **找不到 `pi` 的坑**：OCS 由启动脚本拉起时 PATH 只有 `/usr/local/bin:/usr/bin:/bin`，
  而 pi 装在 `~/.local/bin` → 新增 `resolve_bin()`（显式路径 → PATH → `~/.local/bin` /
  `~/.npm-global/bin` / `~/.local` 等）和 `child_path()`（给子进程补 `~/.local/bin`、
  `~/.npm-global/bin`、`~/.cargo/bin`，让 agent 自己跑的工具也能解析）。
- **审批 UI（两个后端都支持）**：`Event::UiRequest`（select/confirm/input/editor）+
  `Command::UiRespond`；面板在输入框上方渲染警示色提示行：select→选项按钮、
  confirm→允许/拒绝、input/editor→提示到其他客户端作答、取消；`notify` 方法映射为
  Notice 条目。Web 侧 POST `{type:"extension_ui_response",…}` 给 pi-web。
- **实测**：`OCS_PI_MODE=rpc` 启动 OCS → 面板状态行显示「pi rpc · <home>」，
  扩展 notify 通知以 Notice 条目出现；composer 发消息 → 新会话文件
  `~/.pi/agent/sessions/--<home-slug>--/<ts>_<id>.jsonl` 出现 user 消息 +
  assistant thinking/toolCall（即 pi 子进程真的在跑）。auto 模式在有 pi-web 时选中
  「pi-web · http://127.0.0.1:30141」。
- **已知取舍**：pi 的 RPC 子进程里 `@file` **CLI 参数**被禁（与 prompt 文本无关，面板的
  `@` 展开不受影响）；面板窄宽度下 markdown 表格会挤成单字列（→ 已在 §9 修成横向滚动）。

## 12. 后端切换键 + 对比度修正（2026-09-25，提交 `bd64a03e`、`87e07ea6`、其后续）

**背景**：RPC 后端（§11）当时只能用环境变量 `OCS_PI_MODE` 选择，面板里没有任何入口，
用户找不到 → 只能连 pi-web。

**后端选择器（新）**：顶部状态条里，紧跟后端标签之后 ——
`pi-web · http://127.0.0.1:30141  [auto ▾]  ● 生成中，会话 …`

| 选项 | 含义 |
|---|---|
| `auto` | 探测到 pi-web 就用它，否则自动起本机 `pi --mode rpc` |
| `web` | 强制 pi-web（HTTP API） |
| `rpc` | **强制本机 `pi --mode rpc` 子进程，不需要 pi-web** |

- 实现：`PiMsg::BackendPick` → `src/app/update/pi.rs` 里「记住选择 → 停 worker →
  `reset_for_backend_switch()` → 重建 worker（连接中）」。两个后端启动时都会发
  `Event::Backend` + `Event::Sessions`，面板自动跟随最新会话，所以切换后无需手动操作。
- **切换必须清状态**：两个后端的会话表与传输层不同，`reset_for_backend_switch()` 会清掉
  sessions / entries / 模型与思考目录 / 统计 / 待答审批，否则会残留另一后端的数据。
- **持久化**：`~/.config/OpenCADStudio/pi-panel-backend.txt`（`pi::save_backend_mode`）。
  刻意**不并入 `settings.json`** —— Pi 面板整体是 fork 本地扩展，不该扩大共享配置与上游
  冲突面（见 `docs/fork-patches.md` 的 F 组）。`OCS_PI_MODE` 仍是首次启动的默认值；
  读取顺序：偏好文件 → 环境变量 → `auto`。
- 实测：选择器出现在状态条；切换后偏好文件写入对应模式（实测被写成 `web` 后重启即按 web 起）。

**对比度**：新增 `MUTED_TEXT = #B1B1B1`（`Color::from_rgb8(177,177,177)`）常量，替换
主题次级色（深色主题下几乎与背景同色）用于：composer 提示行、用量统计行、**状态条的
会话状态文字**、**思考块正文**（定稿与流式两处）。像素采样：同区域文字峰值从 ~110 提升到 ~157
（9px 小字的抗锯齿峰值低于标称值属正常）。

**Tooltip 样式**：iced 默认 tooltip 是浅色盒 + 继承的白色文字，在深色面板上非常刺眼 →
新增 `tooltip_style()`（底色 `background.weak`、1px `background.strong` 边框、4px 圆角）+ `tooltip_label()`
（`MUTED_TEXT`、10px），面板内三处 tooltip（Auto / Close / 后端选择器）统一使用。后端提示同时压成
**两行短句**（`auto：优先 pi-web，没有就用本机 pi` / `rpc：只跑本机 pi，不需要 pi-web`），
否则单行长提示会横向溢出面板盖到画布上。

**踩坑**：`markdown::Content` 非 `Clone`、`Row` 不换行而是压缩子元素（见 §9）之外，本次新的：
iced 的 `Scrollable` 在所用 rev 里**没有 `max_height()`**（只有 `height()`），要"内容少时自适应、
多时封顶"只能按条目数分支选 `Length::Shrink` / `Length::Fixed(n)`。

## 13. 英文界面（2026-09-25）

面板原本只有中文。现在**跟随宿主语言**：OCS 解析出中文（`zh-CN`/`zh-TW`，或 `LC_ALL`/`LC_MESSAGES`/`LANG` 以 `zh`
开头）→ 中文界面；**其余一律英文**，因此非中文机器上开箱即英文。`OCS_PI_LANG=zh|en` 可强制（截图/测试用）。

实现（全部在 4 个 Pi 文件内，**不新增宿主接点、不动宿主 .ftl 语言目录**）：

- `src/pi.rs` 里的小核心：`Lang` / `lang()`（进程内 `OnceLock` 缓存）/ `tr(zh, en)` /
  `tr_args(zh, en, &[("name", value)])`（`{name}` 占位替换，与宿主 `i18n::translate_args` 同款约定）/
  `panel_title()`。
- 判定顺序：① 宿主 `crate::i18n::loader().current_languages()` 的第一个语言标签（这样**用户在 OCS 设置里改语言
  也会跟着变**）；② 环境变量 `LC_ALL` > `LC_MESSAGES` > `LANG`；③ 都没有 → 英文。
- 停靠面板标题改走 `crate::ui::pi_panel::panel_title()`（原来是字面量 `"Pi 助手"`）。
- 缓存是一次性的：OCS 运行中改语言要重启面板才生效（或用 `OCS_PI_LANG`）。
- 测试断言 UI 文案时用 `crate::pi::tr("中文","English")` 而不是硬编码中文，这样中英环境下都通过。
