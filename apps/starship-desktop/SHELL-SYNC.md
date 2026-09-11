# 壳层同步台账（Shell Sync Log）

本文件记录 `apps/starship-desktop` 与官方 `apps/linux` 的差异，用于回答「以后官方壳更新，我们怎么跟」。

## 基线

| 项 | 值 |
| --- | --- |
| 官方基线 | `openclaw@2026.9.3`，commit `1391f7cd2d` |
| 来源目录 | `apps/linux` |
| 星舰壳版本 | `2.0.8`（复制基线为 `2.0.0`） |
| 复制日期 | 2026-09-10 |

## 已做改动（相对官方 apps/linux）

| 文件 | 改动 | 原因 |
| --- | --- | --- |
| `src-tauri/tauri.conf.json` | `productName=Starship`、`identifier=ai.starship.client`、`version=2.0.8`、窗口标题、窗口 `backgroundColor=#0e1015` | 品牌化 + 消除启动白闪 |
| `src-tauri/Cargo.toml` | `package=starship-desktop`、`bin=starship`、`version=2.0.8` | 品牌化 |
| `src-tauri/Cargo.lock` | 同步 package 名与版本 | 版本一致性 |
| `src-tauri/src/updater.rs` | 更新源改为星舰占位（`updates.starship.invalid`），提示语改 Starship | 防止被官方更新通道覆盖成 OpenClaw |
| `README.md` | 标题改为 Starship Desktop | 品牌化 |

## 壳层原生能力（9.3 壳层从零实现，禁止移植 8.1 源码）

8.1 壳 tag `backup/xb-starship-product-8-1` @ `9d39389c0a` 只作历史参考，不作为移植源。

- [x] `src/native_browser.rs`（原生 WebView2 子视图 + 官方 `openclawBrowser` 协议桥 + 驱动动作层）
  - 单独文件实现，只新增模块、不改官方文件既有行；官方 UI dist 不参与、不被打补丁。
  - 对外协议：官方 Control UI 的 `window.webkit.messageHandlers.openclawBrowser`（沿用官方消息格式，非自定义协议）。
  - 依赖仅 `webview2-com` / `windows` / `idna`（Tauri 既有传递依赖，显式固定版本）。
- [ ] 本地应用发现/启动（按需新增独立文件）
- [ ] Windows 进程树回收（按需新增独立文件）

## 同步规则

1. 官方发新版壳时：`git diff <上一版 apps/linux> <新一版 apps/linux>`，只挑窗口/网关/托盘/更新相关修复。
2. 我们的原生模块放独立文件，**不改官方文件的行**，降低合并冲突。
3. 每次合并在本文件追加一行：日期、官方版本、合并的提交、跳过的提交、原因。
4. UI 层（官方 Control UI）不参与此合并，由网关整体替换。

## 原生视图归属规则（钉死，勿改）

官方 UI 的每个面板实例持有**一个稳定的 presentation scope**（`new ot(e)` 里 `this.scope = l()`，只在实例构造时生成），
而 `present` 里的 `tabId` 取的是**该实例当下的 `activeTargetId`**。同一个 chat pane 换标签时 scope 不变、tabId 变；
不同 pane（dashboard 会把访问过的 pane 全部挂在 DOM 里）则是不同 scope。

官方 `presentation.send()` 用 `lastPayload` 去重：**payload 不变就不重发**。所以壳层一旦丢弃一条 `present`，
那条几何就再也不会补发——这是 2.0.3「右侧面板空白/错位/关不掉」的真因，规则如下：

1. `present` 一律**记录**，绝不因为归属判断而丢弃（丢弃 = 永久丢几何）；跨 scope 的只记日志
   `shell present deferred scope=<对方> live=<当前>`。
2. 归属判断只认**壳层探针**：注入脚本把"屏幕上那个 pane"的 `scope` / `activeTargetId` / `.bp-stage` 几何
   每秒心跳上报。探针新鲜（< `PROBE_FRESHNESS` = 3500ms）且可见时，原生视图**只跟随探针**报的标签与几何；
   官方 `present` 的几何此时只作为诊断信息（`source=probe`）。
3. 探针缺失/过期/不可见时才退回官方 `present` 的顺序裁决（`source=dashboard`），最后才是探针几何兜底，
   保证面板在启动早期也能正常显示。
4. 探针只在 `controller.mode === "interact"` 时上报：官方在非交互模式下自己画视图，原生子视图必须让位。
5. 探针响应速度：250ms 轮询 + 1000ms 心跳 + `ResizeObserver(stage)`（拖动窗口/分隔条时按帧跟随）
   + 点击后 80ms×8 突发重测（点「+」建标签、切标签后由 500ms+ 降到 ~170ms）。

## 变更记录

| 日期 | 官方版本 | 动作 | 说明 |
| --- | --- | --- | --- |
| 2026-09-10 | 2026.9.3 | 建立基线 | 从官方 `apps/linux` 复制并品牌化；`cargo check` + `cargo build` 通过；产出 `starship.exe` 版本 2.0.0 |
| 2026-09-10 | 2026.9.3 | 壳层原生浏览器面板落地（2.0.0 → 2.0.3） | 新增 `src/native_browser.rs`：原生 WebView2 子视图挂载到官方面板 DOM 契约（`openclaw-browser-panel` / `dockLayout.open`）、`openclawBrowser` 协议桥、`task_browser_*` 驱动动作层（elementRef/坐标点击、输入、按键、滚动、inspect、snapshot、CDP 白名单派发）；壳层窗口/网关/托盘改为 `get_window`/`get_webview` 语义、面板握手 `shellDocumentId` 修复空白面板、守卫探测阈值 1500ms → 3500ms |
| 2026-09-10 | 2026.9.3 | 2.0.3 → 2.0.4：导航回退 + 验收固化 | `sanitize_url()` 拦截单标签 `xn--` 主机（punycode 未解码导致 `getaddrinfo ENOTFOUND`）→ `idna::domain_to_unicode()` 解回中文域名，走搜索回退（可用 `STARSHIP_BROWSER_SEARCH_URL` 覆盖）；`open_tab()` 同样过 `sanitize_url()`；新增 `idna` 依赖；三套验收全绿：驱动动作 21/21 PASS、面板开关稳定性 10/10（`"failed": []`）、多 pane 对齐 `"failures": []` |
| 2026-09-10 | 2026.9.3 | 2.0.4：陈旧 scope 抢占修复（原生视图归属改判探针） | 原 `pane_owns_view()` 用「标签 id」判归属，而面板换标签瞬间 `activeTargetId` 与探针不同步 → 合法的 `present` 被丢弃、且官方不重发，旧 scope 一直占位：新建标签后所有驱动动作打在隐藏 WebView2 上（驱动 6 项 FAIL）、面板显现旧标签。改为 `presentation_winners()` 以探针（scope+tab+几何）为唯一归属来源；`present` 全量记录仅记 `shell present deferred` 日志；探针新增 `scope`、`mode=interact` 约束、250ms 轮询 + `ResizeObserver` + 点击突发；几何日志追加 `source=probe|dashboard`。证据：新建标签后 169ms 内 `shell apply tab=mac-… source=probe` 与面板几何逐像素一致；三套验收全绿（驱动 21/21、稳定性 10/10、多 pane `failures: []`、`worstLag=0`） |
| 2026-09-10 | 2026.9.3 | 2.0.4 → 2.0.8 期间：壳层稳定性改动（对应 2.0.5–2.0.7 安装包） | 逐版本对应关系未逐条留证，按代码现状归档：`vendor/wry` + `[patch.crates-io] wry={path="vendor/wry"}` 修 WebView2 COM 回调 panic（崩溃码 `0xc0000409`）；`src/main.rs` 新增 `shell_lifecycle` / `crash_log` 模块（启动阶段事件 + 崩溃日志）；新增 `src/windows_job.rs`，`ensure_outside_job()` 保证星舰自身不被外层 Job Object 连带杀死；`tauri.conf.json` 窗口底色 `#0e1015` 消除启动白闪 |
| 2026-09-11 | 2026.9.3 | 2.0.7 → 2.0.8：浏览器面板对标 Codex + 运行态验证（首次） | 真因：官方组件把样式放在 Lit element styles（落 `shadowRoot.adoptedStyleSheets`），其顺序在任何 append 的 `<style>` 之后 → 同特异性时星舰补丁样式**必输**。修法：①`adoptParitySheet()` 把补丁样式表 concat 到官方 sheet 之后（`WeakMap` 缓存 + 幂等）；②`PARITY_CSS` 竞争规则统一加 `:is(.bp--embedded,.bp--right,.bp--bottom)` 前缀提特异性；③`.bp-header` 改走官方旋钮 `--rail-header-height:32px` / `--rail-header-padding-*` / `--rail-header-background:transparent`（上游重构时自动跟随）；④工具行 `padding 5px 8px→4px 8px`、`gap 4→2`、`.bp-icon 28×28/r6 → 26×26/r7`、`.bp-url` 居中定宽（`flex:1` → `0 1` + `margin:auto`）、`.bp-viewport`/`.bp` `overflow-x:hidden`；⑤`moveNewTabToRail()` 把 URL 框**之前**的建标签按钮移进标签行（order 90），扩展菜单「⋮」置于工具行末（order 99）；⑥扩展菜单 6 项（查找…/放大/缩小/重置缩放/开发者工具/下载文件夹）+ 新增 `act` 动作 `zoom`、`devtools`、`find`、`findStop`、`downloads`，`allowed_cdp_method` 仅额外放开 `Browser.setDownloadBehavior`（不放开整个 `Browser.`）。证据：运行态验证 **56/56 PASS**（Edge headless + CDP 回放真 `INIT_SCRIPT`；脚本 `%TEMP%\starship-parity-verify\verify.mjs`，结果 `result.json` = `total=56 failed=0 ok=true`）；打包 `Starship_2.0.8_x64-setup.exe`；覆盖安装后二进制/注册表均报 `2.0.8` |
| 2026-09-11 | 2026.9.3 | 2.0.9 → 2.0.11：面板行取舍回滚 + 「+」镜像官方清单（2.0.9/2.0.10 未逐条留证） | 2.0.8 之后星舰一度把官方 `.side-panel__header`（「审阅 / 终端 / … / +」那一整行）收起来、再把面板顶到 grid 第一行去凑 Codex 的两行结构；代价是官方「+」里那份面板类型清单（审阅 / 终端 / 浏览器 / 文件 / 侧边聊天 / 任务 / 桌面 / 仪表盘）跟着那一行一起从界面上消失，用户没有第二条路把它叫回来——**用户明确否掉这个取舍**。本批：①那一行原样保留，不再注入任何 host 层隐藏样式，也不再做 content box 抬升；②星舰「+」改为**镜像**官方 `wa-dropdown` 清单：菜单每次打开现读官方 DOM（`side-panel-type-option__label/shortcut/icon`，图标节点深拷贝），点击**转发官方 `wa-dropdown-item` 本体**——官方增删面板类型、改文案快捷键，星舰菜单自动跟随，不维护第二份清单；③清掉 5 处指向已删除函数的悬空引用（`syncHostRail` / `syncHostPanelLift`）：它们让 `scanPanels()` 每次抛 `ReferenceError`，整套 parity 静默失效，症状正是用户看到的「菜单里没有官方那几项」；④新增主文档级 `pointerdown` 关闭（用 `composedPath()` 穿透 shadow 边界认领自己人），修掉「点左侧聊天区菜单关不掉」；⑤修选择器优先级 bug：`STARSHIP_MENU_SELECTOR` 是逗号列表，直接拼 `:not([hidden])` 只修饰最后一段，于是「有菜单开着」恒为真，面板内 Escape 被面板级监听器永久吞掉（查找栏关不掉、官方面板自己的 Esc 也失效）→ 改用 `:is(...):not([hidden])`。证据：运行态验证 **85/85 PASS**（`result.json` = `total=85 failed=0 ok=true`）；真机 `native-browser.log` 出现 `act find` → `act findStop`（Esc 真落到引擎）；重启后 `side-panel__header` 可见、「+」菜单 8 项官方条目全带图标 |
| 2026-09-11 | 2026.9.3 | 2.0.11 → 2.0.12：顶部「+」单入口收敛（去重复按钮） | 2.0.11 之后面板上同时存在两个「+」：第一行官方 `.rail-header__action.side-panel-type-menu__trigger`，第二行星舰搬进标签行的 `data-starship-new-tab`，用户看到「俩个重复啦」。定案（用户原话「保留顶部那个就好了·跟codex一样」）：**只留官方第一行那个 +**，星舰自己那个退成程序化入口。本批：①新增常量 `HOST_ADD_TRIGGER_SELECTOR = "button.side-panel-type-menu__trigger"`；②注入 CSS `.bp-header .bp-icon[data-starship-new-tab]{display:none}` 并**保留 DOM**（键盘/快捷键与 `newTab` 动作仍可走它，只是不占位、不可见）；③星舰菜单浮层锚点 `STARSHIP_MENU_ANCHOR_SELECTOR` 扩为「星舰浮层 + 星舰新标签按钮 + 官方 trigger」三选一，`installMenuDismiss` / `installGlobalMenuDismiss` 一并改用它，修掉「点 + 先关再开」的自关闭竞态；④新增 `installHostAddMenu(panel)`：document 级 `pointerdown` + `click` 捕获，用 `composedPath()` 认官方 `+`，命中即 `preventDefault + stopImmediatePropagation + stopPropagation`（pointerdown 只拦不动作、click 才开关星舰菜单），官方 `wa-dropdown` 因此**完全不弹出**，菜单未装好时就地补跑 `installParity(owner)`；⑤新增 `panelForRailHeader(header)`，按 pane 归属找当前可见的 `openclaw-browser-panel`，多 pane / dashboard 场景不会认错面板。证据：运行态验证 **93/93 PASS**（新增 `/top-plus:*` 断言 8 条：trigger 在位、点击开星舰菜单、镜像 9 项、官方 `wa-dropdown` 保持关闭、二次点击 toggle 关闭、点外部关闭、点条目转发官方本体、选完关闭；`result.json` = `total=93 failed=0 ok=true`）；真机 CDP 复验（端口 9334）：星舰「+」`display:none` / rect `0×0`，官方 trigger `28×28` 在位，真实鼠标点 `(1111,24)` → 星舰菜单 9 项（审阅/终端/浏览器/文件/侧边聊天/任务/桌面/仪表盘/标签页）弹出、官方 8 项下拉 0 项可见，再点一次关闭；打包 `Starship_2.0.12_x64-setup.exe`（4 303 148 字节），覆盖安装后 `%LOCALAPPDATA%\Starship\starship.exe` 版本号 2.0.12 |
| 2026-09-11 | 2026.9.3 | 2.0.12 → 2.0.13：官方行与面板标签行合并为一行（去「重复」） | 2.0.12 收敛掉重复的「+」之后，同一块右侧面板上仍并排站着**两条横栏**：第一行是官方 host 行（面板类型胶囊 + 官方 `+` + 最右关闭），第二行是面板 shadow root 里的 `.bp-header`（网页标签）。用户读到的就是「官方UI和我们内置的还在重复」。定案：**不动 grid、不碰官方监听器**，把面板那条标签行整体上浮一个横栏高（48px）并入官方行，官方控件原位保留。本批（唯一改动文件 `src-tauri/src/native_browser.rs`，+117 行）：①`PARITY_CSS` 新增一组全部挂在 `:host([data-starship-rail-merged])` 下的规则——`.bp-header` 改 `position:absolute; top:calc(-1*var(--starship-rail-height,48px)); left:var(--starship-rail-inset); right:var(--starship-rail-outset); z-index:4; border-bottom:0`，`.bp--embedded` 放开 `overflow`（绝对定位的标签行要探到面板盒子外面），标签胶囊统一 28px 高 / 圆角 14px 对齐官方控件尺码；②新增常量 `HOST_RAIL_ACTIONS_SELECTOR=".side-panel__actions"`、`RAIL_MERGE_CLASS/ATTR`、`RAIL_MERGE_CSS`；③新增 `ensureRailMergeStyle()`（主文档插一次 `style[data-starship-rail-merge]`，放开 `.side-panel__panel` 的裁剪）、`liftTabRowToRail(panel,root)`（实测 `inset = 官方+右边缘 − 面板左边缘 + 6`、`outset = 面板右边缘 − 最右关闭左边缘 + 8`，写自定义属性 + 挂类 + 写 `data-starship-rail-merged`）、`dropRailMerge(panel)`；④`installParity` 在 `moveNewTabToRail` 之后调用；⑤末尾挂 `window.addEventListener("resize", scheduleParityScan)`（复用既有 200ms 防抖 + 2s 扫描，窗口宽度变化时让位量自动重算）。**量不到几何就不写属性**（面板未上屏 / 窗口极窄 / 官方改名），整组规则自动失效退回官方两行——不至于出现半抬半不抬的错位。证据：真机 CDP（端口 9334，`Starship_2.0.13_x64-setup.exe` 覆盖安装后冷启动）：`data-starship-rail-merged="1"`、holder `class="side-panel__panel starship-rail-merged"`、inline `--starship-rail-inset:135px / --starship-rail-outset:48px / --starship-rail-height:48px`；`header.rail-header` `[996,0,472,48]`、`.bp-header` **`[1131,0,289,48]`**（抬进官方行：起点在官方 `+` 右 6px、终点在最右关闭左 8px）、`.bp-toolbar` `[996,48,472,45]`、`.bp-viewport` 高度 1073 → **1121**（多回 48px）；命中测试 `(1150,24)/(1200,24)` 落在自家 `tabstrip-tab__label`、`(1111,24)` 落官方 `+`、`(1442,24)` 落最右关闭、`(1420,24)` 落官方 host 行；真点 `(1111,24)` → 官方清单 9 项（审阅/终端/浏览器/文件/侧边聊天/任务/桌面/仪表盘/标签页）完整弹出、再点关闭；关面板再开，合并自动重算并恢复单行。打包 `Starship_2.0.13_x64-setup.exe`（18 659 840 字节二进制 / 版本 2.0.13）。**9.4 调研（同批）**：npm `2026.9.4` 的 `browser-panel-VJJNnEou.js`（71 978 B）与 9.3 的 `browser-panel-BNJt6USr.js`（70 739 B）对比，`native.*` 成员集合完全一致（`activeTab/beginNewTab/cancelCapture/cancelPendingActivation/capture/connect/disconnect/inspect/mergeRemoteTabs/open/presentation/send/tabs`），`cdp`/`dispatch` 计数均为 0，判定分支仍是 `native.activeTab && mode==='interact'` → `bp-stage--native`，否则截图 + canvas → **9.4 仍未实现原生可交互浏览器**，交互仍归桌面壳层 |
| 2026-09-11 | 2026.9.3 | 2.0.13 → 2.0.15：面板重挂载不再采纳官方未合并的中间态几何（去「点击时跳行闪烁」） | 用户症状：点一下面板里的东西，标签行（合并进官方那一行之后的绝对定位元素）会先掉回第二行、几秒后再弹回第一行。真因：面板重挂载（页面切换 / 隐藏再显示 / 官方自更新）那一瞬间，官方 host 行与面板自己的几何都还没合并完成，壳层按这个中间态算出了让位量并写成属性，等官方布局稳定后又算一次——两次结果不同，眼睛看到的就是「跳一下」。定案：**中间态不记账**，量不到合并态就整组不生效（退回官方两行，宁可短暂两行也不要错位）。来源：这一批未逐条留证，依据为提交 `3f7d9e3d68 starship-desktop 2.0.15: 面板重挂载时不再采纳官方未合并的中间态几何（去点击跳行闪烁）`。 |
| 2026-09-12 | 2026.9.3 | 2.0.15 → 2.0.16：地址栏浏览历史 + 遮挡让位扩展到地址栏下拉 | 用户点名「输入网站这里要有历史浏览记录」（对标 Codex）：地址栏下拉列出访问过的站点，带 favicon + 标题 + hostname；同时把「下拉/浮层遮挡时让位」的规则从面板本体扩展到地址栏下拉这一层，避免下拉被右侧面板或官方 host 行压住。来源：提交 `4da19f0b0c starship-desktop 2.0.16: 地址栏浏览历史（favicon+标题+hostname）+ 遮挡让位扩展到地址栏下拉`（本批未逐条留证）。 |
| 2026-09-12 | 2026.9.3 | 2.0.16 → 2.0.18：①未提交的 P1 批（弹窗原生路由 / `drag` / `upload` / `inputRoute=dom_event` / `COMPUTER_STALE_OBSERVATION` 对齐）；②**弹窗冻结时指针动作回执不再假成功** | 症状：站点弹出 `confirm()` 之后，`act{action:"click"}` 回的是 `{ok:true, effect:"confirmed"}` 却配 `page:null` / `observationId:null`。真因：页面被弹窗冻结时 `Input.dispatchMouseEvent` 那三条命令**照样完成**（排在冻结之前），`perform_act` 于是返回 `Ok`；紧接着 `page_state()` 的 `ExecuteScript` 等满 10s 超时 → `Value::Null` → 回执被拼成「假成功」。上层拿着它继续走，看到的其实是一个已经冻住的页面，10s 后超时兜底还会顺手把弹窗收掉（后续 `act dialog dismiss` 报 `No JavaScript dialog is waiting`）。定案：回执要回答的是「页面**现在**还被弹窗挡着吗」，所以新增 `pending_dialog(tab_id)`（与 `pending_dialog_since` 的区别是**不过滤时间**，每条解弹窗路径——驱动 `dialog`、超时兜底、标签关闭——都会 `clear_pending_dialog`）、`Worker::blocked_dialog(tab_id)`（worker 已入账的 `self.dialogs` 优先，未入账的用跨线程摘要兜底）、`dialog_blocked_receipt(...)`（成功分支与错误分支共用的 `COMPUTER_DIALOG_BLOCKED` builder）。`Ok(detail)` 分支加闸：有弹窗压着就直接回 `COMPUTER_DIALOG_BLOCKED`，错误文案 `the page is frozen on a {kind} dialog, so the effect could not be observed`；**顺带省掉那 10 秒注定超时的观测**。证据：同一条探针用例修复前 `10139ms` → `ok:true / page:null`，修复后 **`126ms` → `COMPUTER_DIALOG_BLOCKED`**（带完整 `dialog:{kind:"confirm",message:"starship-confirm",uri:...}`），后续 `act dialog dismiss confirms` 随之转绿；dev 与装机版各跑一遍 `probe-link-routing.mjs`（41 条 ALL PASS）/ `browser-driver-regression.mjs`（42 条 ALL PASS）/ `browser-panel-stability.mjs`（6/6，settle ~170ms）/ `probe-dialog-timeout.mjs`（9/9 ALL PASS）/ `browser-multipane-regression.mjs`（`failures: []`），装机版二进制 FileVersion **2.0.18**。**探针加固（同批）**：`browser-panel-stability.mjs` 的第 4 个位置参数是 `logPath`，却被当成其它套件的 `fixturePort` 传了 `19202`，于是探针**一个日志字节都没读到**，却仍报出「6 轮全部超时」这种足以让人以为壳层回归的结论（`fallbackDuringTest present:0 cleared:0`）——现在改成：4 号位是纯数字就当端口忽略、日志文件不存在或为空直接 `exit(2)` 报错，不再用假失败浪费时间。打包 `Starship_2.0.18_x64-setup.exe`（4 368 037 字节 / 版本 2.0.18），覆盖安装后 `%LOCALAPPDATA%\Starship\starship.exe` 版本号 2.0.18 |
