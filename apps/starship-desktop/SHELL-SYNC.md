# 壳层同步台账（Shell Sync Log）

本文件记录 `apps/starship-desktop` 与官方 `apps/linux` 的差异，用于回答「以后官方壳更新，我们怎么跟」。

## 基线

| 项 | 值 |
| --- | --- |
| 官方基线 | `openclaw@2026.9.3`，commit `1391f7cd2d` |
| 来源目录 | `apps/linux` |
| 星舰壳版本 | `2.0.4`（复制基线为 `2.0.0`） |
| 复制日期 | 2026-09-10 |

## 已做改动（相对官方 apps/linux）

| 文件 | 改动 | 原因 |
| --- | --- | --- |
| `src-tauri/tauri.conf.json` | `productName=Starship`、`identifier=ai.starship.client`、`version=2.0.4`、窗口标题 | 品牌化 |
| `src-tauri/Cargo.toml` | `package=starship-desktop`、`bin=starship`、`version=2.0.4` | 品牌化 |
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
