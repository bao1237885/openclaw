# 官方 9.3 Computer Use v2 ↔ 星舰 task_browser 面板 · 对接映射表

用途：回答「官方 9.3 的计算机操作契约里，哪些动作走官方、哪些仍由星舰面板自持、哪些需要补兼容层」。
本方为**兼容层设计基线**：官方契约改动时，先比对第 1、2 节，再决定改壳层还是改兼容层。

## 0. 定位与铁律

- 官方契约唯一来源：`src/plugins/computer-use-contract.ts`（`COMPUTER_USE_V2_ACTION_NAMES` = **40 个动作**，L10–L51；`COMPUTER_USE_V1_ACTION_NAMES` = `slice(0,15)`；`COMPUTER_ACT_V1_ACTION_NAMES` = `slice(1,14)`）。
- 星舰只做「嵌入式 WebView2 视图 + 驱动动作层」，**不动官方 `ui/` 源码**；面板 DOM 契约沿用官方 `openclaw-browser-panel`，样式补丁走注入层（顺序必须排在官方 `adoptedStyleSheets` 之后）。
- 映射原则：官方 UI 自己能渲染/自己能做到的一律交给官方；星舰只补官方**没有**的那一域（真 WebView2 嵌入 + 可驱动动作）。

## 1. 官方浏览器族动作（9 个）→ 星舰壳

官方参数见契约 L205–L304，浏览器族统一带 `browserRef` + `pageRef`（星舰侧对应 `scope` + `tabId`）。

| 官方动作 | 官方参数要点 | 星舰壳入口 | 现状 |
| --- | --- | --- | --- |
| `get_browser_state` | `snapshotFormat: dom_refs_v1 \| semantic_v2`、`elementRef`、`continuation` | `elements` + `snapshot` | **部分**：`dom_refs_v1` 语义已对齐；`semantic_v2`、`continuation` 未实现 |
| `browser_prepare` | `windowRef`、`profile: isolated_new \| isolated_named`、`profileName` | `open`（WebView2 用户数据目录即 profile 语义） | **部分**：隔离 profile 的命名/复用未做 |
| `browser_navigate` | `url` | `navigate` | **已有** |
| `browser_click` | `observationId`、`elementRef` 或 `x/y`、`inputRoute: trusted \| dom_event` | `act{action:"click", elementRef\|x,y, button, clickCount}` | **已有**（固定走 CDP trusted；未提供 `dom_event` 退化路径） |
| `browser_type` | `elementRef`、`text`、`mode: insert_text \| keystrokes`、`replace` | `act{action:"type"}` + `act{action:"key"}` | **部分**：`replace`、逐键 `keystrokes` 未做 |
| `browser_pointer` | `pointerAction: hover \| right_click \| double_click \| scroll \| drag`，含 `destinationElementRef`/`toX,toY`/`deltaX,deltaY` | `act{action:"hover"\|"move"\|"scroll"\|"click"}` | **部分**：hover ✅、right_click ✅（`button=right`）、double_click ✅（`clickCount=2`）、scroll ✅；**drag ❌ 未实现** |
| `browser_dialog` | `dialogAction: inspect \| accept \| dismiss`、`dialogRef`、`promptText` | `act{action:"dialog", mode:"accept"\|"dismiss", promptText}`；`inspect` 读面板状态里标签的 `dialog` 元数据 | **已有**（壳层原生路由，见 §4.1） |
| `browser_set_input_files` | `elementRef`、`resourceHandles`（1–32 个，形如 `openclaw:computer-resource:v1:<uuid>`） | — | **缺**：需 `DOM.setFileInputFiles` + 资源句柄→本地路径解析；星舰壳层目前没有资源仓储 |
| `browser_download` | `observationId`、`elementRef` | `downloads` | **部分**：「打开下载文件夹」已有（`Browser.setDownloadBehavior`）；由 `elementRef` 触发的下载动作未做 |

## 2. 非浏览器动作（31 个）：归官方 CUA 节点，星舰不重复实现

屏幕/窗口/应用级：`screenshot`、`left_click`…`wait`（V1 前 15 个）、`list_apps`、`list_windows`、`get_accessibility_tree`、`get_cursor_position`、`get_window_state`、`launch_app`、`kill_app`、`bring_to_front`、`set_value`、`zoom`、`escalate_scope`、`get_recording_state` / `start_recording` / `stop_recording`、`replay_trajectory`、`invoke_menu`。

这些是「控制整个桌面」的动作，走官方 node host；星舰壳只在**内嵌 WebView2 面板**这一域补驱动，避免两套实现互相打架。

### ⚠️ 同名不同义（兼容层必须分流）

| 名字 | 官方语义 | 星舰语义 |
| --- | --- | --- |
| `zoom` | **屏幕区域放大观察**：`windowRef` + `observationId` + `x1,y1,x2,y2`（契约 L197–L204） | **网页缩放**：`act{action:"zoom", direction:in\|out\|reset}`，clamp 0.25–3.0、step 0.1 |

禁止把两个 `zoom` 直接透传；兼容层按「是否带 `x1..y2`」分流。

## 3. 星舰壳现有入口（已实现，附证据行号）

| 层 | 内容 | 位置 |
| --- | --- | --- |
| 请求 kind | `open` / `navigate` / `back\|forward\|reload\|stop` / `close` / `snapshot` / `elements` / `dispatch` / `act` / `inspect` / `present` / `release-scope` | `src-tauri/src/native_browser.rs` L1830–L2070 |
| act 动作 | `click`(left\|right\|middle, clickCount 1–3) / `hover\|move` / `scroll` / `type` / `key\|press` / `wait` / `screenshot` / `snapshot` / `elements` / `navigate` / `back\|forward\|reload\|stop` / `zoom` / `devtools` / `find` / `findStop` / `downloads` / `drag` / `upload` / `dialog` | 同文件 `Command::Act` → `perform_act` |
| CDP 白名单 | 前缀 `Input.` / `Page.` / `DOM.` / `Runtime.` / `Network.` + **EXACT** `Browser.setDownloadBehavior`（刻意不放开整个 `Browser.`） | 同文件 L2836 |
| 桥协议 | 上行 `{__starship:true,id,message}`；下行 `{__starshipReply:true,id,reply}` / `{__starshipState:true,state}`；20s 超时 | 同文件 `INIT_SCRIPT` |
| 注入暴露 | `window.openclawBrowserAct` / `openclawBrowserDispatch` / `openclawBrowserElements` / `window.webkit.messageHandlers.openclawBrowser` / `CustomEvent openclaw:native-browser-state` | 同文件 `INIT_SCRIPT` |

## 4. 错误码对齐

官方：`COMPUTER_CONTRACT_MISMATCH`、`COMPUTER_STALE_OBSERVATION`（契约 L59–L60）。
星舰：**已对齐** `COMPUTER_CONTRACT_MISMATCH`（动作名不认识）、`COMPUTER_STALE_OBSERVATION`（`observationId` 过期，回包带当前 `observationId`）。
另有壳层自有的 `COMPUTER_DIALOG_BLOCKED`：动作落到一个正被站点弹窗挡住的标签上，回包附 `dialog{kind,message,defaultText,uri}`，
上层据此发 `act{action:"dialog"}`，而不是重试原动作。其余失败仍是 `{ok:false, error:"<文本>"}`。

### 4.1 站点弹窗（alert / confirm / prompt / beforeunload）：为什么必须由壳层接管

默认情况下 WebView2 会给站点弹窗拉起自己的模态框。子 WebView2 是**挂在 Tauri 窗口上的子视图**，
那个框既不属于面板、也没有关闭入口，而渲染进程会一直等它 —— 面板表现为「这个标签死了」：
`Runtime.evaluate` / `Runtime.enable` / `DOM.getDocument` 全部超时，只有 `Page.*` 还活着。
`Page.handleJavaScriptDialog` 在这状态下回的是 `No dialog is showing`，拿它当药方等于什么都没做。

壳层的做法（全在 `native_browser.rs`）：

1. 建标签时 `SetAreDefaultScriptDialogsEnabled(false)`，再订阅 `ScriptDialogOpening`；
2. 回调里**只做三件事**：先 `GetDeferral()`（攥住「什么时候放行」的决定权），读 `Kind`/`Message`/`DefaultText`/`Uri` 记账，投 `Command::ScriptDialog`。
   回调里绝不跑消息循环、绝不等回包（微软文档明确警告）；
3. 弹窗元数据随 `push_state` 上屏（标签的 `dialog` 字段），放行用的 COM 手柄留在壳层；
4. `act{action:"dialog"}` 走原生路由：`complete_native_dialog` 必须用 `with_webview` 跳到 WebView2 自己的线程再查手柄
   —— worker 线程读到的 thread_local 是另一份空的；
5. `DIALOG_AUTO_DISMISS = 10s` 必须有：攥着 deferral 不放，等于把「WebView2 弹框卡死」换成「壳层卡死」。
   非 `beforeunload` 一律按**取消**收（自动按「确定」是替用户答应站点），`beforeunload` 按 accept 走；
6. 同一标签再冒一个弹窗：旧的那个先 `Complete()` 放行（等同取消），不能丢 —— 丢了页面永远卡在上一轮。

验收：`probe-dialog-timeout.mjs`（超时兜底 9/9 PASS）、`probe-link-routing.mjs`（弹窗上屏 → 可解除 → 页面按 cancel 继续，ALL PASS）。

## 5. P0 验收标准

> agent 在星舰嵌入式面板上，可带 `elementRef`/`x,y` 点击/输入/滚动，观察回传 `effect=confirmed`。

现状：**已达标**。证据：驱动动作验收 21/21 PASS；面板对标运行态验证 **85/85 PASS**（真 `INIT_SCRIPT` + 真 Chromium 回放，
`%TEMP%\starship-parity-verify\verify.mjs`，`result.json` = `total=85 failed=0 ok=true`，2026-09-11）。

## 6. 缺口清单（按优先级）

| 优先级 | 缺口 | 说明 |
| --- | --- | --- |
| 已完成（2.0.8） | 面板视觉对标 Codex、`zoom`、`devtools`、`find`/`findStop`、`downloads` | 见 `SHELL-SYNC.md` 2.0.8 行 |
| 已完成（2.0.11） | 官方「审阅/终端/…/+」那一行原样保留 + 星舰「+」镜像官方面板清单（现读现用） | 见 `SHELL-SYNC.md` 2.0.11 行；同一批修掉「Escape 被面板级监听器永久吞掉」 |
| P1 | ~~`browser_dialog`~~（已完成）、~~`browser_set_input_files`~~（已完成，仅本地路径）、~~`browser_pointer:drag`~~（已完成）、~~`inputRoute=dom_event`~~（已完成）、~~`COMPUTER_STALE_OBSERVATION`~~（已完成）；剩 `semantic_v2`、`continuation` | 都在壳层 `native_browser.rs` 内可做，不动官方源码 |
| P1（面板功能面） | 文件上传、网站权限（摄像头/定位/剪贴板）、代理设置 | 用户明确点名的对标项 |
| P2（需裁决） | 标签拖拽排序 | 官方 `panel-tab-strip` 支持 `onReorder`，但唯一调用点 `browser-panel-tabs.ts` 没传；且浏览器面板**不在**插件可替换 surface（仅 `session-list`/`composer`/`workspace`/`transcript`/`tool-result`）→ 只能走上游 PR 或受控小补丁 |
| P2 | 壳层两项：本地应用发现/启动、Windows 进程树回收 | `SHELL-SYNC.md` 仍为 `[ ]` |
