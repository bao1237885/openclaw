# 星舰客户端开发治理策略 v2.0

> 一句话路线：**官方 OpenClaw 9.3 当底座（引擎 + Control UI 主界面），我们只做两件事——壳层改装（Tauri）和 UI 插件增量（Control UI Plugin）。不 fork 官方 UI，不往官方产物打补丁。**
>
> 本版是唯一有效版本，v1.0 中与本节冲突的表述一律作废。

---

## 0. 本版改了什么（v1.0 → v2.0）

v1.0 存在**方向自相矛盾**：第二节写「自研 `starship-ui` 完全顶替官方 UI」，第五节写「官方 UI 当宿主 + 插件增量」。本版统一为**后者**，并把前者连同它衍生的所有做法列入废弃清单（第 9 节）。

同步修正的三处：

1. 基线从「`product-8.1` 壳当唯一权威」改为「官方 9.3 壳改品牌」。
2. 「`apps/starship-desktop` 已不在仓库、需重建脚手架」改为「壳源码在本地 tag，需**恢复并移植**」。
3. 新增第 1 节「两条版本轴」，这是之前所有混乱的根因。

---

## 1. 两条版本轴（必须先分清，否则永远在原点打转）

| 轴           | 内容                                                                 | 当前值                                     | 谁维护               | 用户能看见吗                             |
| ------------ | -------------------------------------------------------------------- | ------------------------------------------ | -------------------- | ---------------------------------------- |
| **A 引擎轴** | `openclaw` npm 引擎 + 官方 Control UI（网关 `127.0.0.1:18791` 提供） | `2026.9.3`                                 | 官方，我们只跟正式版 | **能**，聊天/任务/设置等主界面都在这一层 |
| **B 壳层轴** | Tauri 桌面壳（窗口、托盘、网关守护、原生 WebView2 浏览器面板）       | 星舰 `1.0.0`（基于 8.1）→ 目标基于官方 9.3 | 我们自己             | 外壳、启动页、原生面板                   |

**关键结论：壳层基于 8.1 还是 9.3，不决定用户看到的 UI。**
用户看到的对话/任务界面来自 A（引擎侧官方 Control UI）。所以：

- 壳层换成官方 9.3 基线 → 只是换「外壳」，UI 仍是官方 9.3 Control UI；
- 不存在「从 8.1 开始，UI 就还是 8.1」这回事；
- 8.1 那套「自研 UI 顶替官方 + `starship-zh-inject.js` DOM 注入」才是反复崩溃的根源，本次**不移植**。

---

## 2. 当前真实基线（2026-09-10 实测）

| 项             | 实测值                                                             | 证据                                                                     |
| -------------- | ------------------------------------------------------------------ | ------------------------------------------------------------------------ |
| 引擎版本       | `2026.9.3`                                                         | `C:\Users\36042\.openclaw\tools\node\node_modules\openclaw\package.json` |
| 当前源码分支   | `starship/9.3-clean` @ `1391f7cd2d`                                | `git -C C:\Users\36042\openclaw-source log -1`                           |
| 官方壳位置     | `apps\linux`（`OpenClaw / 0.1.0 / ai.openclaw.linux`）             | `apps\linux\src-tauri\tauri.conf.json`                                   |
| 运行中的客户端 | `C:\Program Files\Starship\starship.exe`，`FileVersion=1.0.0`      | 文件属性                                                                 |
| 星舰壳源码     | **只存在本地 tag** `backup/xb-starship-product-8-1` @ `9d39389c0a` | `git for-each-ref`                                                       |
| 远程           | fork 已有 `starship/9.3-clean`，**无任何星舰壳分支**               | `refs/remotes/fork/*`                                                    |

**风险**：星舰壳源码目前只靠本地 git tag 存活，未推远程，有丢失风险 → 第 5 节治理。

---

## 3. 目标架构（四层）

```
┌──────────────────────────────────────────────────────────────┐
│ L3  星舰 UI 插件（ControlUiPlugin）★ 我们唯一的 UI 增量入口     │
│      任务看板 · 连接器 · 编排 · 成本仪表盘 · 浏览器入口         │
│      形态：descriptor(pluginId/revision/entryUrl/styles)      │
└──────────────────────────▲───────────────────────────────────┘
                           │ 官方插件宿主加载（不碰官方源码）
┌──────────────────────────┴───────────────────────────────────┐
│ L2  官方 Control UI（网关 18791 提供）★ 主界面，保留不动       │
│      composer / workspace / transcript / session-list /       │
│      tool-result surface + 导航路由 + 官方组件                 │
└──────────────────────────▲───────────────────────────────────┘
                           │ Gateway HTTP/WS API（稳定契约）
┌──────────────────────────┴───────────────────────────────────┐
│ L1  星舰壳层（Tauri v2 + WebView2）★ 只做窗口与原生能力        │
│      窗口/托盘 · 网关守护 · 原生浏览器面板(task_browser/CDP)   │
│      源码：apps/starship-desktop（从官方 apps/linux 改品牌）   │
└──────────────────────────▲───────────────────────────────────┘
                           │ 拉起 / 配置
┌──────────────────────────┴───────────────────────────────────┐
│ L0  官方引擎（openclaw@9.3 npm）★ 不改源码                     │
│      行为修正 = runtime 补丁清单(.runtime-patch.json)          │
│      升级 = 换 npm 版本 + 按清单重放，不碰源码                 │
└──────────────────────────────────────────────────────────────┘
```

数据流两条，互不耦合：

- **UI 增量**：插件 ← `ControlUiHost` → Gateway API。
- **原生浏览器**：UI 入口（插件）→ 壳层 IPC → `task_browser.rs` → WebView2 / CDP。

---

## 4. 冲突隔离铁律（违反即回到旧坑）

### 三不

1. **不改官方 `ui/` 源码**（包括 `browser-panel*`）。
2. **不往官方 `dist/` 打补丁**（官方升级 = 整体替换，补丁必丢）。
3. **不把官方组件拷进我们仓库再改**。

### 三要

1. **UI 增量一律走 `ControlUiPlugin`**（surface / route / component）。
2. **原生能力放壳层**（真实 WebView2、托盘、网关守护、系统集成）。
3. **只通过稳定契约对接**：Gateway HTTP/WS API、`@openclaw/plugin-sdk/control-ui`、壳层 IPC。

### 官方升级时会发生什么

| 层         | 升级行为                   | 对我们影响                     |
| ---------- | -------------------------- | ------------------------------ |
| L0 引擎    | 换 npm 版本 + 重放补丁清单 | 接口可能漂移 → 加兼容层        |
| L2 官方 UI | 官方产物**整体替换**       | 我们不碰它，插件被宿主重新加载 |
| L1 壳层    | 我们自己的源码             | 不动                           |
| L3 插件    | 独立模块                   | 只需跟踪 plugin-sdk 契约版本   |

---

## 5. 分支与仓库治理

### 5.1 分支/tag 命名（固定）

| 名称                                   | 用途                                      |
| -------------------------------------- | ----------------------------------------- |
| `starship/product-9.3`                 | **唯一正式开发分支**，从官方 9.3 基线开出 |
| `starship/baseline-9.3` (tag)          | 官方 9.3 基线快照                         |
| `starship-shell/v1.0.0-8.1` (tag)      | 现有 1.0.0 壳归档（指向 `9d39389c0a`）    |
| `openclaw-upstream-archive/<version>/` | 每次正式版上游整仓备份                    |

> 分支名 `product-9.3` 待用户最终确认；若坚持 `product-8.1`，内容也必须是 9.3 基线，名字只是标签。

### 5.2 壳源码防丢（P0）

现有壳只存在本地 tag。立即做两件事：

1. 打归档 tag `starship-shell/v1.0.0-8.1`；
2. 远程恢复后，把 `starship/product-9.3` 分支 + 全部 `starship*` tag 推到 fork。

### 5.3 远程现状

- `origin` = `https://gh-proxy.com/https://github.com/openclaw/openclaw.git`
- `fork` = `https://gh-proxy.com/https://github.com/bao1237885/openclaw.git`
- 当前网络下 `github.com:443` 直连超时、`gh-proxy.com` SSL EOF → **推送需等网络恢复**，本地先落分支和 tag。

### 5.4 壳层与官方壳的同步策略（回答「以后还会不会冲突」）

壳层（L1）是**我们自己的 fork**，跟官方 `apps/linux` 的关系是「同源、可择要合并」，不是「自动跟随」：

- 官方壳更新时，用 `git diff 上一版官方 apps/linux 新一版官方 apps/linux` 看变更，**只挑与窗口/网关/托盘相关的修复**，人工合进 `apps/starship-desktop`。
- 我们自己新增的原生模块（`task_browser.rs`、`app_host.rs`）放在独立文件，**不改官方文件的行**，所以合并时几乎不会冲突。
- 每次合并记录在 `apps/starship-desktop/SHELL-SYNC.md`：官方版本、合了哪些提交、跳过了哪些、为什么。
- UI 层（L2）**完全不参与这个合并**，官方 UI 由网关整体替换，壳层不需要跟。

---

## 6. 分阶段执行计划

### 阶段 0：9.3 壳基线 + 品牌化（1–2 天）

**目标**：拿到一个能编译、能启动、能连网关的官方 9.3 壳，品牌为 Starship。

| #   | 动作                                                                    | 命令/位置                                                                                                                                                                 |
| --- | ----------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0.1 | 从 `starship/9.3-clean` 开正式分支                                      | `git switch -c starship/product-9.3`                                                                                                                                      |
| 0.2 | 复制 `apps/linux` → `apps/starship-desktop`，**保留官方结构，最小改动** | `git show 1391f7cd2d:apps/linux`                                                                                                                                          |
| 0.3 | 改品牌四处（缺一打出来还是旧版本号）                                    | ① `tauri.conf.json` 的 `productName/identifier/version`；② `Cargo.toml` 的 `package.name/version` 和 `[[bin]].name`；③ `Cargo.lock`；④ 源码里 `env!("CARGO_PKG_VERSION")` |
| 0.4 | 编译验证                                                                | `cargo build --manifest-path apps/starship-desktop/src-tauri/Cargo.toml`                                                                                                  |
| 0.5 | 启动验证：能连 `18791` 官方 Control UI                                  | 手动                                                                                                                                                                      |
| 0.6 | **选择性移植** 8.1 壳的原生模块（只补官方壳缺的，不覆盖官方已有的）     | 见下方清单                                                                                                                                                                |

**0.6 移植清单（关键：只搬官方没有的）**

| 模块                                                                        | 处理                        | 原因                                     |
| --------------------------------------------------------------------------- | --------------------------- | ---------------------------------------- |
| `task_browser.rs`                                                           | **移植**                    | 官方壳没有；星舰原生 WebView2 浏览器核心 |
| `app_host.rs`                                                               | **移植**                    | 官方壳没有；本地应用发现/启动            |
| `job_object.rs`                                                             | **移植**                    | 官方壳没有；Windows 进程树回收           |
| `canvas.rs` / `mcp.rs`                                                      | 评估后移植                  | 看是否被我们的 UI 插件依赖               |
| `gateway.rs` / `gateway_ws.rs` / `tray.rs` / `updater.rs` / `quickchat*.rs` | **不移植，用官方 9.3 版本** | 官方已有且更新，覆盖会带回旧 bug         |
| `starship-zh-inject.js` / 自研 UI 顶替                                      | **禁止移植**                | 已列入废弃清单                           |

**验收**：

- [ ] 安装后 exe `FileVersion` = 目标版本（读安装后的 exe，不看安装包文件名）
- [ ] 启动后加载官方 Control UI（不是我们自己的静态页）
- [ ] 冷启动 ≤ 10s

**风险**：官方壳目录名是 `linux`，`Cargo.toml` 的 `cfg(target_os="windows")` 分支存在但要实测；先跑通再改名。

### 阶段 1：官方 UI 保真 + 浏览器桥归位（2–3 天）

**目标**：浏览器面板不再跳独立窗口、不白屏；官方 UI 完整。

| #   | 动作                                                                                 |
| --- | ------------------------------------------------------------------------------------ |
| 1.1 | 确认官方 UI 由网关提供（`18791`），壳只负责加载与导航                                |
| 1.2 | 原生浏览器面板改为壳层 WebView2 子视图（移植 `task_browser.rs`），**不注入官方 DOM** |
| 1.3 | 面板 UI 入口走 `ControlUiPlugin`（挂 `workspace` surface）                           |
| 1.4 | 打包并校验浏览器面板可打开、可驱动                                                   |

**验收**：

- [ ] 点浏览器入口 → 出现壳层原生面板（非截图、非独立弹窗）
- [ ] 连续开关 10 次不崩
- [ ] 冷启动 ≤ 10s

**历史根因（已定位）**：1.0.0 打包时用了官方 npm dist，`apps/starship-desktop/ui/assets/browser-panel-BNW001UL.js` 与官方 `openclaw@2026.8.1` 同名文件 **sha256 完全一致** → 源码里的 task-browser 桥被打包流程丢了。修法：**从源码构建 UI，不拷官方 dist**。

### 阶段 2：可交互浏览器 P0（3–5 天）

**目标**：agent 能驱动面板点击/输入/滚动，对标 Codex 内置浏览器。

| #   | 动作                                                     | 位置                                                  |
| --- | -------------------------------------------------------- | ----------------------------------------------------- |
| 2.1 | `task_browser_cdp_dispatch` 通用 CDP 命令                | `apps/starship-desktop/src-tauri/src/task_browser.rs` |
| 2.2 | `task_browser_act` 统一入口（elementRef / x,y / scroll） | 同上                                                  |
| 2.3 | `act()` / `dispatch()`                                   | `native-browser.ts`                                   |
| 2.4 | 前端面板接 `computer.act` 语义                           | 面板前端                                              |
| 2.5 | 回归用例：点击/输入/滚动/截图全链路                      | 金线任务集                                            |

**验收**：agent 带 `elementRef` 点击 → 观察回传 `effect=confirmed`。

### 阶段 3：星舰 UI 插件（持续）

**目标**：所有增量 UI 都是 `ControlUiPlugin`，不碰官方。

| 功能         | 接入点                         | 形式                      |
| ------------ | ------------------------------ | ------------------------- |
| 任务看板定制 | `workspace` / `session-list`   | 替换面                    |
| 连接器管理   | `ControlUiPageTarget` + `path` | 自建导航页                |
| 编排         | `workspace`                    | 替换面                    |
| 成本仪表盘   | `dashboard`                    | 复用官方组件              |
| 浏览器入口   | `workspace`                    | UI 走插件，原生面板在壳层 |

**插件开发工作流**：建插件（`@openclaw/plugin-sdk/control-ui` 类型）→ 实现 `views`/`replacements`/`routes` → 出 `descriptor` → 挂官方宿主自测 → 每个接入点配一条回归用例。

**验收**：官方 UI 产物整体替换后，插件自动重载，无需改官方源码。

### 阶段 4：治理与稳定（持续）

- 只跟正式版；每季度按第 7 节 8 步升级。
- 源码归档必做。
- 插件瘦身（禁用用不到的 provider 插件，降冷启动和内存）。
- 会话中断根治（Transcript 冲突、网关重启丢投递）。
- DeepSeek V4 Flash 缓存命中 ≥ 97%（长会话靠缓存命中省钱）。
- 金线任务集回归：主链路可用才允许发版。

---

## 7. 升级 8 步流程（正式版发布时执行）

1. **确认**官方正式版编号（changelog + 版本号，不跟 beta）。
2. **归档源码**（必做）：上游整仓备份到 `openclaw-upstream-archive/<version>/`。
3. **升依赖**：更新 `openclaw` 到目标正式版。
4. **重放补丁**：算新 dist `fingerprint` → 逐条重放 `.runtime-patch.json` 的 `appliedPatchIds` → 上游已实现的补丁移除。
5. **金线回归**：任务闭环（写代码/修代码）、模型路由、浏览器面板可驱动。
6. **插件契约检查**：`@openclaw/plugin-sdk/control-ui` 是否有 breaking change，有则只改插件适配层。
7. **打 tag + CHANGELOG**：`v<内核>.<产品>.<patch>`。
8. **发版 + 干净环境安装验证**：过第 8 节清单。

### runtime 补丁清单

文件：`apps/starship-desktop/runtime/.runtime-patch.json`

```json
{
  "patchSetVersion": 1,
  "version": "1.0.0",
  "fingerprint": "<SHA256 of engine dist>",
  "appliedPatchIds": [
    "starshipBrowserPanelBridge",
    "starshipSessionKeepAlive",
    "starshipWindowsPortFix"
  ]
}
```

每条补丁 = 一个**独立、小、可回滚**的行为修正，严禁与上游同路径文件大改。

---

## 8. 发布前例行校验清单

- [ ] 冷启动 ≤ 10s
- [ ] 会话窗口正常收发，无中断 / 白屏 / 闪退
- [ ] 浏览器面板可驱动（点击 / 输入 / 滚动，`elementRef` / `x,y` 生效）
- [ ] 模型接入正常（DeepSeek / GLM / 自有代理）
- [ ] 安装后 exe `FileVersion` = 目标版本
- [ ] 官方升级后插件零改动（或仅适配层改动）

---

## 9. 废弃项清单（禁止回退）

| 废弃做法                              | 为什么                                                   | 替代                         |
| ------------------------------------- | -------------------------------------------------------- | ---------------------------- |
| 自研 UI 完全顶替官方 UI               | 反复崩、每次升级冲突                                     | 官方 UI 宿主 + 插件          |
| `starship-zh-inject.js` DOM 注入      | 官方一改 DOM 就崩                                        | 插件 surface / 官方 i18n     |
| 往官方 dist 打补丁 / 拷官方组件回来改 | 升级即被覆盖                                             | `ControlUiPlugin`            |
| 从官方 npm dist 拷 UI 产物            | 丢源码里的桥（1.0.0 的 browser-panel sha256 与官方一致） | 从源码构建 + 插件            |
| `product-8.1` 当唯一权威壳            | 基线过旧，带着旧 UI 包袱                                 | `product-9.3` 壳基线         |
| 跟 beta / 每日构建                    | 不稳定                                                   | 只跟正式版                   |
| 官方 UI 组件树里塞浏览器面板          | 官方升级覆盖桥 → 跳窗/白屏/100×100 卡死                  | 壳层原生 WebView2 + 插件入口 |

---

## 10. 立即执行的第一步（本轮）

1. 提交本策略文档（`STARSHIP-DEVELOPMENT-STRATEGY.md`）。
2. 确认分支命名 `starship/product-9.3`（默认采用）。
3. 执行阶段 0.1–0.3：开分支、复制官方壳、改品牌。
4. 编译验证（先确保能 build）。
5. 远程恢复后推送分支 + tag（当前网络受限，本地先落）。

---

## 附录 A：命令速查

```powershell
# 壳源码 tag 内容
git -C C:\Users\36042\openclaw-source ls-tree -r --name-only 9d39389c0a -- apps/starship-desktop/src-tauri/src
git -C C:\Users\36042\openclaw-source show 9d39389c0a:apps/starship-desktop/src-tauri/tauri.conf.json

# 全部星舰相关 refs
git -C C:\Users\36042\openclaw-source for-each-ref --format='%(refname) %(objectname:short)' | Select-String 'star|product|browser|backup'

# 运行中进程 / exe 版本
Get-Process -Name "*starship*" | Select-Object Id,Path
(Get-Item 'C:\Program Files\Starship\starship.exe').VersionInfo

# 引擎版本
(Get-Content C:\Users\36042\.openclaw\tools\node\node_modules\openclaw\package.json -Encoding UTF8 | ConvertFrom-Json).version
```

## 附录 B：证据索引

| 结论                                 | 证据文件                                                                                                                            |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------- |
| 官方 UI 是插件宿主                   | `src/plugin-sdk/control-ui.ts`、`ui/src/plugins/control-ui-{host,loader,runtime,view,contributions}.ts`                             |
| 官方浏览器只实现 Apple 端            | `ui/src/app/native-browser-bridge.ts`、`native-browser-host.ts`（`window.webkit.messageHandlers.openclawBrowser`）                  |
| 官方壳是 companion + 打开 Control UI | `apps/linux/README.md`、`apps/linux/ui/main.js`                                                                                     |
| 1.0.0 打包丢桥                       | `C:\Users\36042\.openclaw\agents\main\workspace\bridge\starship-to-codex\codex-task-browser-bridge-20260901-0745.md`（sha256 对比） |
| 壳源码在 tag                         | `backup/xb-starship-product-8-1` @ `9d39389c0a`                                                                                     |

---

_最后更新：2026-09-10 · 版本 v2.0 · 唯一有效_
