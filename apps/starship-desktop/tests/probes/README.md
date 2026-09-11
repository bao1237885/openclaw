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
node browser-multipane-regression.mjs 9334 chat/main /chat/main/<id-a> 10000 /chat/main/<id-b>
```

## 各套件覆盖什么

| 套件 | 断言 | 盯住的回归 |
| --- | --- | --- |
| `probe-link-routing.mjs` | 41 | 左键点 `target="_blank"` 之类链接时的标签归属；导航是否留在同一标签而不是新开/跳走 |
| `browser-driver-regression.mjs` | 42 | `act` / `dispatch` / `elements` 桥的语义：点击、输入、滚动、elementRef 与坐标两条路线、回执结构与 `effect` |
| `browser-panel-stability.mjs` | 6 轮 | 面板反复开关后的几何稳定：每轮 `applied` 是否落值、settle 时间、`fallback` 是否被误触发 |
| `probe-dialog-timeout.mjs` | 9 | `confirm` / `alert` / `prompt` 行为：冻结时动作回执必须是 `COMPUTER_DIALOG_BLOCKED`，解弹窗后能继续 |
| `browser-multipane-regression.mjs` | 失败列表 | 多窗格同时开面板时几何互不串位、`scope` 不抢占 |

`shell-geometry.mjs` 是共用工具（读壳层日志的事件流、比对矩形、判定让位量），
被稳定性与多窗格两套 import，单独跑没有意义。

## 一处需要记住的坑

第四个位置参数在**别的套件里**是夹具服务器端口（`19199` / `19201` / `19099`），
但在 `browser-panel-stability.mjs` 里是**日志路径**。曾经照别的套件习惯传了
`19202`，探针于是一个日志字节都没读到，却照样报出「6 轮全部超时」——
足以让人误判壳层回归。现在该套件对纯数字的第 4 参只看不认（打印提示后改用
默认日志），日志不存在或为空直接 `exit(2)`，不再用假失败浪费时间。

## 判定口径

探针只回答「现在是否偏离基线」，不回答「为什么」。出现失败时先看壳层日志同一
时间窗的 `shell apply tab=... source=probe` 事件，再决定是壳层回归还是探针
自身参数/环境问题——上面那条坑就是靠这个顺序识破的。
