# 壳层嵌入式浏览器回归探针

这套探针通过 CDP 驱动**运行中的星舰客户端**，验证原生 WebView2 浏览器面板的
路由、驱动、几何、弹窗与多窗格行为。它们不启动应用，也不改代码——只观察和
施加真实输入，因此可以作为每次改壳层前后的同一把尺子。

跑之前需要三样东西都在：

1. 网关在 `127.0.0.1:18791`（客户端正常登录即是）
2. 客户端带远程调试端口启动（`--remote-debugging-port=9334`，见仓库同级
   `launch-target-9334.cmd` / `launch-installed-9334.cmd`）
3. 壳层日志可读：
   `C:\Users\36042\AppData\Local\ai.starship.client\native-browser.log`

Node 直接跑，不需要依赖安装（只用 node 内置模块）：

```powershell
cd C:\Users\36042\openclaw-source\apps\starship-desktop\tests\probes
node probe-link-routing.mjs        9334 chat/main 19199
node browser-driver-regression.mjs 9334 chat/main 19201
node browser-panel-stability.mjs   9334 chat/main 6
node probe-dialog-timeout.mjs      9334 chat/main 19099 14000
node probe-downloads.mjs           9334 chat/main 19211
node probe-agent-io.mjs            9334 chat/main 18997
node browser-multipane-regression.mjs 9334 chat/main /chat/main/<id-a> 10000 /chat/main/<id-b>
```

另有一把**不连客户端**的尺子，改 `native_browser.rs` 里的 JS 之后先跑它：

```powershell
cd C:\Users\36042\openclaw-source\apps\starship-desktop
node tests\check-injected-scripts.mjs
```

它从 Rust 源码里抠出全部 `= r#"…"#` JS 块，只交给 `node:vm` 解析（不执行）。
存在的理由：`PARITY_CSS` 注释里曾有一个未转义反引号提前闭合了模板串，让**整段
3254 行** `INIT_SCRIPT` 变成语法错误、星舰注入层整个不装（`window.webkit`
不存在），而 `cargo build` 全绿——只有语法检查能拦。正常输出 `ALL PASS (13 blocks)`。

## 各套件覆盖什么

| 套件 | 断言 | 盯住的回归 |
| --- | --- | --- |
| `probe-link-routing.mjs` | 39 | 左键点 `target="_blank"` 之类链接时的标签归属；导航是否留在同一标签而不是新开/跳走 |
| `browser-driver-regression.mjs` | 66 | `act` / `dispatch` / `elements` 桥的语义：点击、输入、滚动（含逐键 `keystrokes` / `replace`）、elementRef 与坐标两条路线、`semantic_v2` 分页与观测序号校验、回执结构与 `effect` |
| `browser-panel-stability.mjs` | 6 轮 | 面板反复开关后的几何稳定：每轮 `applied` 是否落值、settle 时间、`fallback` 是否被误触发 |
| `probe-dialog-timeout.mjs` | 9 | `confirm` / `alert` / `prompt` 行为：冻结时动作回执必须是 `COMPUTER_DIALOG_BLOCKED`，解弹窗后能继续 |
| `probe-downloads.mjs` | 22 | 下载账本与落盘目录：壳层报的目录必须等于 Windows 记录的下载文件夹、「刚下完的文件」能从面板 reveal（越权路径要被拒）、⋮「下载」浮层能被让位且不越出窗口边缘、清空账本 |
| `probe-agent-io.mjs` | 79 | 智能体原生交互五组：地址栏历史（下拉行 / `ArrowDown` + `Enter` / 真实鼠标点击，在**没有活动标签**时都必须开出一个标签）、`select` 的 value·label·index 三条成功路径与四条拒绝路径、`hover` 的 `rewarmed` 首帧兜底与页面 `:hover` 真落上、动作可视化层（虚拟光标 / 涟漪 / 滚动指示）以及它**不被算成遮挡**（带标记的假浮层不许触发替身让位——那是白屏与抖动的老路径）、**人机共存**（用户与智能体同时在同一个页面上干活，只撞到同一块地方才让路：同点让路而别处照走、观察类根本不进闸、写字挡的是那个框、滚动恒让路；反向还要证壳层给自己盖的章没把智能体自己的输入记成人手） |
| `browser-multipane-regression.mjs` | 失败列表 | 多窗格同时开面板时几何互不串位、`scope` 不抢占 |

`probe-agent-io.mjs` 会**关掉面板里全部标签**：第 1 组的前提就是「零活动标签」，
只留一个标签就复现不了用户报的那条。所以它跟 `probe-downloads.mjs` 一样自带夹具
服务器（默认 `18997`），跑完自己收尾。

`probe-downloads.mjs` 与其它套件不同的一点：面板没有活动标签时它会自己 `open`
一个，不依赖上一轮留下的会话状态；并且它跑前跑后都会清掉 `starship-dl-probe.bin`
残留（两处目录都扫）——否则上一跑的文件会让 WebView2 把本次存成 `… (1).bin`，
后面每条「文件名对不对」的断言都在问另一个文件。

`shell-geometry.mjs` 是共用工具（读壳层日志的事件流、比对矩形、判定让位量），
被稳定性与多窗格两套 import，单独跑没有意义。

## 一处需要记住的坑

驱动类套件都靠**当前活动标签**干活。面板里一个标签都没有时，`.bp-stage` 量不到
矩形，第一句「the visible pane presents a browser panel」直接 FAIL——而这条 FAIL
长得像壳层回归。所以 `browser-driver-regression.mjs` / `probe-link-routing.mjs`
跑之前，先确认面板里至少有一个标签（`probe-agent-io.mjs` 与 `probe-downloads.mjs`
自己会 `open`，不受这条约束）。

比这更隐蔽的一处：这些套件是用 `url.includes("chat/main")` 找**壳层窗口**的。
如果留在面板里的标签 URL 恰好也含 `chat/main`（比如把面板开到
`http://127.0.0.1:18791/`，它会跳到 `/chat/main/dashboard/…`），探针就会挑中那个
标签当壳层，然后对一个没有 `__OPENCLAW_NATIVE_BROWSER__` 的页面发问，症状同样是
「面板没挂上」。留下活动标签时给个中性地址（夹具页、
`127.0.0.1:18999/index.html` 这类）就不会踩到。

第四个位置参数在**别的套件里**是夹具服务器端口（`19199` / `19201` / `19099`），
但在 `browser-panel-stability.mjs` 里是**日志路径**。曾经照别的套件习惯传了
`19202`，探针于是一个日志字节都没读到，却照样报出「6 轮全部超时」——
足以让人误判壳层回归。现在该套件对纯数字的第 4 参只看不认（打印提示后改用
默认日志），日志不存在或为空直接 `exit(2)`，不再用假失败浪费时间。

## 判定口径

探针只回答「现在是否偏离基线」，不回答「为什么」。出现失败时先看壳层日志同一
时间窗的 `shell apply tab=... source=probe` 事件，再决定是壳层回归还是探针
自身参数/环境问题——上面那条坑就是靠这个顺序识破的。
