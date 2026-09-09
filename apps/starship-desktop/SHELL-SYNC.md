# 壳层同步台账（Shell Sync Log）

本文件记录 `apps/starship-desktop` 与官方 `apps/linux` 的差异，用于回答「以后官方壳更新，我们怎么跟」。

## 基线

| 项 | 值 |
| --- | --- |
| 官方基线 | `openclaw@2026.9.3`，commit `1391f7cd2d` |
| 来源目录 | `apps/linux` |
| 星舰壳版本 | `2.0.0` |
| 复制日期 | 2026-09-10 |

## 已做改动（相对官方 apps/linux）

| 文件 | 改动 | 原因 |
| --- | --- | --- |
| `src-tauri/tauri.conf.json` | `productName=Starship`、`identifier=ai.starship.client`、`version=2.0.0`、窗口标题 | 品牌化 |
| `src-tauri/Cargo.toml` | `package=starship-desktop`、`bin=starship`、`version=2.0.0` | 品牌化 |
| `src-tauri/Cargo.lock` | 同步 package 名与版本 | 版本一致性 |
| `src-tauri/src/updater.rs` | 更新源改为星舰占位（`updates.starship.invalid`），提示语改 Starship | 防止被官方更新通道覆盖成 OpenClaw |
| `README.md` | 标题改为 Starship Desktop | 品牌化 |

## 待移植（来自 8.1 壳 tag `backup/xb-starship-product-8-1` @ `9d39389c0a`）

- [ ] `src/task_browser.rs`（原生 WebView2 + CDP）
- [ ] `src/app_host.rs`
- [ ] `src/job_object.rs`
- [ ] 评估 `src/canvas.rs`、`src/mcp.rs`

## 同步规则

1. 官方发新版壳时：`git diff <上一版 apps/linux> <新一版 apps/linux>`，只挑窗口/网关/托盘/更新相关修复。
2. 我们的原生模块放独立文件，**不改官方文件的行**，降低合并冲突。
3. 每次合并在本文件追加一行：日期、官方版本、合并的提交、跳过的提交、原因。
4. UI 层（官方 Control UI）不参与此合并，由网关整体替换。

## 变更记录

| 日期 | 官方版本 | 动作 | 说明 |
| --- | --- | --- | --- |
| 2026-09-10 | 2026.9.3 | 建立基线 | 从官方 `apps/linux` 复制并品牌化；`cargo check` + `cargo build` 通过；产出 `starship.exe` 版本 2.0.0 |
