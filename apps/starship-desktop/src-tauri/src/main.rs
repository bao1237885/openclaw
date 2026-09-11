// Hide the console window in release builds (the official linux shell never
// needed this; on Windows a console-subsystem exe launches with a black
// terminal that kills the app when closed).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod cli;
mod discovery;
mod gateway;
mod gateway_device_identity;
mod gateway_operation_queue;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
mod gateway_sleep;
#[cfg(target_os = "linux")]
mod gateway_sleep_logind;
#[cfg(target_os = "linux")]
mod gateway_sleep_logind_listener;
mod gateway_ws;
mod installer;
mod notify;
mod native_browser;
mod pending_approvals;
mod quickchat;
mod quickchat_widgets;
mod remote_gateway;
mod tray;
mod updater;
mod windows_job;

use cli::{CliError, OpenClawCli};
use gateway::{GatewayAction, GatewaySnapshot, ReadyGateway};
use gateway_operation_queue::{GatewayOperation, GatewayOperationQueue};
use installer::InstallChannel;
use remote_gateway::RemoteGatewayRequest;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::webview::{NewWindowResponse, WebviewBuilder};
use tauri::{
    AppHandle, Emitter, LogicalPosition, Manager, State, Url, Webview, WebviewUrl, Window,
    WebviewWindowBuilder,
};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_global_shortcut::{Code, Modifiers};
use tauri_plugin_opener::OpenerExt;

const CONNECTED_WATCH_INTERVAL: Duration = Duration::from_secs(15);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);
/// Approval polling shells out to the CLI twice per round (`nodes pending`,
/// `devices list`), so it runs on its own thread every fourth connected tick
/// instead of blocking the reachability loop.
const APPROVAL_POLL_TICKS: u64 = 8;
/// Consecutive "port open but no HTTP answer" ticks the watchdog tolerates
/// before it is allowed to boot the CLI. Four ticks is one minute of grace for
/// a Gateway that is merely saturated.
const STALLS_BEFORE_CLI: u32 = 4;
/// The CLI fallback costs a full Node boot, so it is rate limited and backs off
/// while the Gateway keeps failing: 2 min, 4 min, 8 min, capped at 10 min.
const CLI_BACKOFF_BASE: Duration = Duration::from_secs(120);
const CLI_BACKOFF_MAX: Duration = Duration::from_secs(600);

fn cli_backoff(attempts: u32) -> Duration {
    (CLI_BACKOFF_BASE * (1u32 << attempts.min(3))).min(CLI_BACKOFF_MAX)
}
fn external_browser_url_allowed(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
}

fn native_auth_initialization_script(
    dashboard: &Url,
    gateway: &Url,
    request: &RemoteGatewayRequest,
) -> Result<String, String> {
    if request.transport == "direct" && request.tls_fingerprint.is_some() {
        return Err(
            "The desktop dashboard cannot securely verify a pinned Gateway TLS certificate. \
             Connect using Remote over SSH instead."
                .to_string(),
        );
    }
    let path = dashboard.path().trim_end_matches('/');
    let origin = serde_json::to_string(&dashboard.origin().ascii_serialization())
        .map_err(|_| "Could not prepare secure Gateway authentication.".to_string())?;
    let path = serde_json::to_string(if path.is_empty() { "/" } else { path })
        .map_err(|_| "Could not prepare secure Gateway authentication.".to_string())?;
    let auth = serde_json::json!({
        "gatewayUrl": gateway.as_str(),
        "token": request.token,
        "password": request.password,
    });
    let auth = serde_json::to_string(&auth)
        .map_err(|_| "Could not prepare secure Gateway authentication.".to_string())?;
    Ok(format!(
        r#"(() => {{
  try {{
    if (location.origin !== {origin}) return;
    const base = {path};
    if (base !== "/" && location.pathname !== base && !location.pathname.startsWith(`${{base}}/`)) return;
    Object.defineProperty(window, "__OPENCLAW_NATIVE_CONTROL_AUTH__", {{
      value: {auth},
      configurable: true,
    }});
  }} catch {{}}
}})();"#
    ))
}

fn open_external_browser(app: &AppHandle, url: &Url) {
    if external_browser_url_allowed(url)
        && app.opener().open_url(url.as_str(), None::<&str>).is_err()
    {
        eprintln!("Could not open the external sign-in page.");
    }
}

/// 诊断开关：只有开发通道打开时才给 WebView2 开 DevTools 协议端口，好让本地会话读到
/// 真实 DOM（星舰的注入层只跑在 Tauri webview 里，用普通 Edge 打开 18791 是复现不出来的）。
/// 端口来源依次是环境变量 STARSHIP_WEBVIEW_DEBUG_PORT、命令行 `--starship-dev-cdp=`、
/// 以及 `dev/cdp-port.txt`。
///
/// 为什么必须写在代码里：Tauri 总会自带一串 browser arguments，而 WebView2 只要拿到
/// 代码传入的参数就会完全忽略 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS，
/// 所以以往靠环境变量开 CDP 的做法在本壳上一直是无效的。
/// 变量不存在时返回 None，正式运行时行为与之前完全一致。
///
/// 这是**全进程唯一**的参数来源，主 WebView 和每个子 WebView（浏览器面板标签、快捷
/// 聊天、发现窗口）都必须调它。不是代码洁癖，而是 WebView2 的硬约束：同一份
/// user data folder 上，两次创建只要 `CoreWebView2EnvironmentOptions` 不一致，后一次
/// 就会直接失败（wry `web_context.rs` 写明了这条平台限制）。实测后果是子标签永远停在
/// "正在加载页面"，而 `window.add_child()` 仍返回 Ok —— 因为
/// `tauri-runtime-wry` 把创建失败只写进 log、不回传调用方。所以参数必须同源，而且在
/// 同一进程里只解一次：主 WebView 会因重连/恢复被重建，每次重解就可能和已经开着的
/// 子视图漂移成两套参数。
pub(crate) fn webview_debug_browser_args() -> Option<String> {
    static ARGS: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ARGS.get_or_init(|| {
        let port = dev_switch("STARSHIP_WEBVIEW_DEBUG_PORT", "starship-dev-cdp")
            .or_else(|| dev_file("cdp-port.txt"))?;
        let port = port.trim();
        if port.is_empty() || !port.chars().all(|digit| digit.is_ascii_digit()) {
            return None;
        }
        // 这里的 --disable-features 与 Tauri 的默认值保持一致：一旦我们显式传入参数，
        // WebView2 不会再叠加默认值，漏掉就会让默认行为被改掉。
        dev_log(&format!("webview debug port requested: {port}"));
        Some(format!(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={port}"
        ))
    })
    .clone()
}

/// 开发期固定目录：`%LOCALAPPDATA%\ai.starship.client\dev`。
///
/// 为什么最后落到「固定路径的哨兵文件」：星舰壳在本机是由 WMI / 计划任务这类路径
/// 拉起的，实测这种启动方式**既传不进环境变量，也传不进命令行参数**（进程的
/// CommandLine 里看不到我们传的 `--starship-dev-*`，环境变量同样丢失）。
/// 目录是否存在这个事实和启动方式完全无关，所以它是唯一可靠的开关载体。
/// 正式用户机器上不会存在这个目录，因此正式运行时行为与以前完全一致。
fn dev_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let dir = std::path::PathBuf::from(base)
        .join("ai.starship.client")
        .join("dev");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

/// `dev/` 下哨兵文件的**内容**（读不到或为空返回 None）。
///
/// 这一层是开发机上唯一稳定生效的开关来源：它对 WMI / 计划任务 / 快捷方式 /
/// 单实例转发全都免疫，而环境变量和命令行参数在这些启动路径下都会丢。
fn dev_file(name: &str) -> Option<String> {
    let value = std::fs::read_to_string(dev_dir()?.join(name)).ok()?;
    let value = value.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// 开发期开关的「显式」取值入口：环境变量 → 命令行参数。
///
/// 这两个来源传进来的都是**取值本身**（CDP 端口号）或**路径**（外挂脚本位置），
/// 前者由调用方再退到 `dev_file`，后者由调用方自己读文件。
fn dev_switch(variable: &str, flag: &str) -> Option<String> {
    if let Ok(value) = std::env::var(variable) {
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    let prefix = format!("--{flag}=");
    std::env::args()
        .skip(1)
        .find_map(|argument| {
            argument
                .strip_prefix(&prefix)
                .map(|value| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
}

/// 开发期诊断日志：`dev/` 目录存在时追加写到 `dev/log.txt`。
///
/// 存在的意义：星舰壳经常由外部方式拉起（计划任务 / WMI / 快捷方式），
/// 环境变量可能在中途被吞掉，只看界面根本判断不出“热更通道有没有被读到”。
/// 正式运行不会有 dev 目录，所以不会在用户机器上留下任何文件。
fn dev_log(message: &str) {
    let Some(dir) = dev_dir() else {
        return;
    };
    let path = dir.join("log.txt");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write as _;
        let _ = writeln!(file, "{message}");
    }
}

/// 注入层（native_browser.rs 里的 INIT_SCRIPT）是唯一编译进壳的部分，导致每改一行
/// 浏览器 UI 都要重新打包。这里给开发期留一个外挂：`dev/parity.js` 存在时，
/// 就用它的内容代替编译进壳的脚本，于是改样式只需重启客户端（不重新打包）。
/// 文件不存在或读不到时，行为与之前完全一致（始终用编译进壳的版本）。
fn parity_init_script() -> String {
    dev_log("parity init script requested");
    if let Some(path) = dev_switch("STARSHIP_PARITY_SCRIPT", "starship-dev-parity-script") {
        let path = path.trim();
        if !path.is_empty() {
            match std::fs::read_to_string(path) {
                Ok(source) if !source.trim().is_empty() => {
                    eprintln!("[dev] parity script override: {path}");
                    dev_log(&format!("parity override used: {path} ({} chars)", source.len()));
                    return source;
                }
                Ok(_) => {
                    eprintln!("[dev] parity script override is empty, using built-in: {path}");
                    dev_log(&format!("parity override empty: {path}"));
                }
                Err(error) => {
                    eprintln!("[dev] parity script override unreadable ({error}), using built-in: {path}");
                    dev_log(&format!("parity override unreadable: {path} ({error})"));
                }
            }
        } else {
            dev_log("parity override path was blank");
        }
    }
    // 固定路径哨兵：文件内容是脚本本体（不是路径），所以直接拿来用。
    // 这是开发机上唯一靠得住的覆盖方式：启动方式怎么变都不影响。
    match dev_file("parity.js") {
        Some(source) => {
            dev_log(&format!("parity override file used: {} chars", source.len()));
            return source;
        }
        None => dev_log("parity override file absent, using built-in script"),
    }
    native_browser::INIT_SCRIPT.to_string()
}

/// 原生浏览器**每个标签文档**的注入层（面板骨架之外的第二份，见
/// `native_browser::TAB_INIT_SCRIPT`）。
///
/// 和 `parity_init_script` 用同一套开发期外挂：`dev/native-tab.js` 存在时整段替换。
/// 这一层的改动不需要重新编译，也不需要重启客户端——它挂在文档创建时，所以改完文件
/// 只要让标签导航一次（打开新标签或刷新）就生效。改「页面里的行为策略」走这条路，
/// 改「面板骨架」走 `dev/parity.js`。
pub(crate) fn native_browser_tab_script() -> String {
    if let Some(source) = dev_file("native-tab.js") {
        if !source.trim().is_empty() {
            dev_log(&format!("native tab script override used: {} chars", source.len()));
            return source;
        }
    }
    native_browser::TAB_INIT_SCRIPT.to_string()
}

fn is_active_onboarding_url(url: &Url) -> bool {
    let path = url.path().trim_end_matches('/');
    let query_key = if path.ends_with("/settings/model-setup") {
        "firstRun"
    } else if path.ends_with("/custodian") {
        "onboarding"
    } else {
        return false;
    };
    url.query_pairs()
        .find(|(key, _)| key == query_key)
        .is_some_and(|(_, value)| {
            if query_key == "firstRun" {
                return value == "1" || value == "explicit";
            }
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BuildInfo {
    version: String,
    release_build: bool,
}

fn is_release_version(version: &str) -> bool {
    // The committed 0.1.0 version identifies branch builds; release builds are stamped by CI.
    version != "0.1.0"
}

// The openclaw:// URL contract is deliberately tiny and handled entirely in
// Rust: `openclaw://dashboard` opens/connects the dashboard; anything else
// just focuses the app. New routes are added to this enum — the renderer
// (which is often navigated away to the remote dashboard) never sees URLs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeepLinkRoute {
    Dashboard,
    FocusOnly,
}

fn deep_link_route(url: &Url) -> DeepLinkRoute {
    if url.scheme() == "openclaw" && url.host_str() == Some("dashboard") {
        DeepLinkRoute::Dashboard
    } else {
        DeepLinkRoute::FocusOnly
    }
}

fn handle_deep_links(app: &AppHandle, urls: Vec<Url>) {
    for url in urls {
        match deep_link_route(&url) {
            DeepLinkRoute::Dashboard => {
                tray::open_dashboard(app);
            }
            DeepLinkRoute::FocusOnly => tray::show_window(app),
        }
    }
}

#[cfg(test)]
mod deep_link_tests {
    use super::{deep_link_route, DeepLinkRoute, Url};

    #[test]
    fn dashboard_route_matches_only_the_openclaw_dashboard_host() {
        let dashboard = Url::parse("openclaw://dashboard/ignored?source=test").unwrap();
        let other = Url::parse("openclaw://settings/dashboard").unwrap();
        let other_scheme = Url::parse("https://dashboard/").unwrap();

        assert_eq!(deep_link_route(&dashboard), DeepLinkRoute::Dashboard);
        assert_eq!(deep_link_route(&other), DeepLinkRoute::FocusOnly);
        assert_eq!(deep_link_route(&other_scheme), DeepLinkRoute::FocusOnly);
    }
}

#[cfg(test)]
mod native_browser_tests {
    use super::{
        external_browser_url_allowed, native_auth_initialization_script, RemoteGatewayRequest, Url,
    };
    use std::process::Command;

    #[test]
    fn oauth_browser_accepts_http_urls_but_never_unsafe_schemes_or_userinfo() {
        for (candidate, allowed) in [
            (
                "https://auth.openai.com/oauth/authorize?state=fixture",
                true,
            ),
            ("http://127.0.0.1:1455/auth/callback", true),
            ("file:///etc/passwd", false),
            ("javascript:alert(1)", false),
            ("data:text/html,fixture", false),
            ("openclaw://dashboard", false),
        ] {
            assert_eq!(
                external_browser_url_allowed(&Url::parse(candidate).expect("URL")),
                allowed,
                "unexpected external-browser decision for {candidate}"
            );
        }
        let userinfo = ["operator", "fixture"].join(":");
        let credentialed = format!("https://{userinfo}@gateway.example.com");
        assert!(!external_browser_url_allowed(
            &Url::parse(&credentialed).expect("credentialed URL")
        ));
    }

    #[test]
    fn native_password_handoff_is_origin_scoped_and_consumed_before_page_code() {
        let request = RemoteGatewayRequest {
            transport: "direct".to_string(),
            url: Some("https://gateway.example.com/openclaw".to_string()),
            ssh_target: None,
            token: None,
            password: Some("fixture-password".to_string()),
            remote_port: None,
            tls_fingerprint: None,
        };
        let dashboard = Url::parse("https://gateway.example.com/openclaw").expect("dashboard");
        let gateway = Url::parse("wss://gateway.example.com/openclaw").expect("Gateway");
        let initialization_script =
            native_auth_initialization_script(&dashboard, &gateway, &request).expect("auth script");
        assert!(!dashboard.as_str().contains("fixture-password"));
        assert!(!gateway.as_str().contains("fixture-password"));

        let runner = r#"
            const init = new Function('window', 'location', process.argv[1]);
            const cases = [
              ['https://gateway.example.com', '/openclaw', true],
              ['https://gateway.example.com', '/openclaw/settings/model-setup', true],
              ['https://attacker.example.com', '/openclaw', false],
              ['https://gateway.example.com', '/openclaw-other', false],
              ['https://gateway.example.com', '/other', false],
            ];
            for (const [origin, pathname, allowed] of cases) {
              const window = {};
              init(window, {origin, pathname});
              const auth = window.__OPENCLAW_NATIVE_CONTROL_AUTH__;
              if (Boolean(auth) !== allowed) throw new Error('origin/path policy failed');
              if (allowed && (auth.gatewayUrl !== 'wss://gateway.example.com/openclaw' || auth.password !== 'fixture-password')) {
                throw new Error('native password was not delivered');
              }
            }
        "#;
        let output = Command::new("node")
            .args(["-e", runner, &initialization_script])
            .output()
            .expect("Node is required by the OpenClaw workspace");
        assert!(
            output.status.success(),
            "native auth handoff failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn pinned_remote_gateway_never_receives_credentials_through_an_unpinned_webview() {
        let request: RemoteGatewayRequest = serde_json::from_value(serde_json::json!({
            "transport": "direct",
            "url": "https://gateway.example.com/openclaw",
            "token": "fixture-token",
            "tlsFingerprint": "ab".repeat(32),
        }))
        .expect("pinned remote request");
        let dashboard = Url::parse("https://gateway.example.com/openclaw").expect("dashboard");
        let gateway = Url::parse("wss://gateway.example.com/openclaw").expect("Gateway");
        let result = native_auth_initialization_script(&dashboard, &gateway, &request);

        assert!(
            result.is_err(),
            "a certificate-pinned Gateway must never receive credentials through an unpinned WebView"
        );
        assert!(
            result
                .err()
                .expect("rejected pin")
                .contains("Remote over SSH"),
            "the rejection must explain the secure supported transport"
        );

        let mut tunneled = request;
        tunneled.transport = "ssh".to_string();
        let tunneled_dashboard = Url::parse("http://127.0.0.1:18789").expect("tunneled dashboard");
        let tunneled_gateway = Url::parse("ws://127.0.0.1:18789").expect("tunneled Gateway");
        assert!(
            native_auth_initialization_script(&tunneled_dashboard, &tunneled_gateway, &tunneled)
                .is_ok(),
            "host-key-verified SSH tunneling must remain available"
        );
    }
}

#[derive(Default)]
struct NavigationState {
    // One lock owns both fields so the intent check and WebView navigation cannot interleave.
    remote_dashboard: bool,
    watch_generation: u64,
    onboarding_pending: bool,
}

impl NavigationState {
    fn cancel_watchdog(&mut self) {
        self.watch_generation = self.watch_generation.wrapping_add(1);
    }

    fn select_remote(&mut self) {
        self.cancel_watchdog();
        self.remote_dashboard = true;
    }

    fn permit_local(&mut self, force: bool, expected_generation: Option<u64>) -> bool {
        if expected_generation.is_some_and(|expected| expected != self.watch_generation) {
            return false;
        }
        if self.remote_dashboard && !force {
            return false;
        }
        if force {
            self.cancel_watchdog();
            self.remote_dashboard = false;
        }
        true
    }

    fn begin_watchdog(&mut self) -> Option<u64> {
        if self.remote_dashboard {
            return None;
        }
        self.cancel_watchdog();
        Some(self.watch_generation)
    }

    fn watchdog_is_current(&self, generation: u64) -> bool {
        !self.remote_dashboard && self.watch_generation == generation
    }

    fn mark_onboarding_pending(&mut self) {
        self.onboarding_pending = true;
    }

    fn prepare_dashboard_url(&mut self, target: &str) -> Result<Url, String> {
        let mut url =
            Url::parse(target).map_err(|_| "Dashboard returned an invalid URL.".to_string())?;
        if self.onboarding_pending {
            // Setup owns inference before chat; preserve Gateway base paths and fragment auth.
            // Saved first-run links may use either marker; new links use explicit.
            url.path_segments_mut()
                .map_err(|_| "Dashboard returned an invalid URL.".to_string())?
                .pop_if_empty()
                .extend(["settings", "model-setup"]);
            let existing_query = url
                .query_pairs()
                .filter(|(key, _)| key != "firstRun")
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            url.query_pairs_mut()
                .clear()
                .extend_pairs(existing_query)
                .append_pair("firstRun", "explicit");
            self.onboarding_pending = false;
        }
        Ok(url)
    }
}

struct DesktopInner {
    cli: Mutex<Option<OpenClawCli>>,
    navigation: Mutex<NavigationState>,
    operation: Mutex<()>,
    pending_approvals: Mutex<pending_approvals::PendingApprovalState>,
    approvals_polling: AtomicBool,
    local_url: Url,
    tray: Mutex<Option<tray::TrayHandles>>,
    remote_tunnel: Mutex<Option<remote_gateway::SshTunnel>>,
    quitting: AtomicBool,
}

#[derive(Clone)]
pub struct DesktopState {
    inner: Arc<DesktopInner>,
}

impl DesktopState {
    fn new(local_url: Url) -> Self {
        Self {
            inner: Arc::new(DesktopInner {
                cli: Mutex::new(None),
                navigation: Mutex::new(NavigationState::default()),
                operation: Mutex::new(()),
                pending_approvals: Mutex::new(pending_approvals::PendingApprovalState::default()),
                approvals_polling: AtomicBool::new(false),
                local_url,
                tray: Mutex::new(None),
                remote_tunnel: Mutex::new(None),
                quitting: AtomicBool::new(false),
            }),
        }
    }

    fn set_tray(&self, handles: tray::TrayHandles) {
        *self.inner.tray.lock().expect("tray mutex poisoned") = Some(handles);
    }

    pub(crate) fn set_quickchat_shortcut_checked(&self, checked: bool) {
        if let Some(tray) = self
            .inner
            .tray
            .lock()
            .expect("tray mutex poisoned")
            .as_ref()
        {
            tray.set_quickchat_shortcut_checked(checked);
        }
    }

    pub fn connect(&self, app: &AppHandle) -> Result<GatewaySnapshot, String> {
        self.connect_selected(app, false)
    }

    fn connect_selected(
        &self,
        app: &AppHandle,
        explicit_local: bool,
    ) -> Result<GatewaySnapshot, String> {
        let _operation = self
            .inner
            .operation
            .lock()
            .map_err(|_| "Gateway operation lock is unavailable.".to_string())?;
        if !explicit_local {
            if let Some(remote) = remote_gateway::load_saved_remote()? {
                return self.connect_remote_locked(app, remote);
            }
        }
        // Fast path: the local Gateway may already be listening. Answering that
        // from `openclaw.json` plus one loopback request keeps cold start in the
        // hundreds of milliseconds; `openclaw gateway status --json` boots the
        // full Node CLI (plugins, config audit, MCP discovery) and costs 20s+
        // on a large install.
        if let Ok(Some(probe)) = gateway::local_gateway_probe() {
            if probe.reachable() {
                if explicit_local {
                    self.inner
                        .remote_tunnel
                        .lock()
                        .map_err(|_| "Remote Gateway tunnel lock is unavailable.".to_string())?
                        .take();
                }
                return self.finish_connection(app, None, probe.ready());
            }
        }
        let cli = self.resolve_cli();
        if !explicit_local && !remote_gateway::has_configured_gateway()? {
            // First-run setup belongs to the pending bootstrap reply. Navigating
            // here replaces its WebView and loses the local/remote choice.
            let snapshot = match cli {
                Ok(_) => GatewaySnapshot::unconfigured(),
                Err(CliError::Missing) => GatewaySnapshot::missing_cli(),
                Err(error) => return Err(error.to_string()),
            };
            self.update_tray(&snapshot);
            return Ok(snapshot);
        }
        let cli = match cli {
            Ok(cli) => cli,
            Err(CliError::Missing) => {
                return self.show_missing_cli(app, explicit_local, None);
            }
            Err(error) => return Err(error.to_string()),
        };
        if explicit_local {
            self.inner
                .remote_tunnel
                .lock()
                .map_err(|_| "Remote Gateway tunnel lock is unavailable.".to_string())?
                .take();
        }
        let ready = gateway::ensure_ready(&cli)?;
        self.finish_local_connection(app, cli, ready)
    }

    pub fn install_cli(
        &self,
        app: &AppHandle,
        channel: InstallChannel,
    ) -> Result<GatewaySnapshot, String> {
        let _operation = self
            .inner
            .operation
            .lock()
            .map_err(|_| "Installer lock is unavailable.".to_string())?;
        installer::install(app, channel)?;
        let cli = OpenClawCli::discover().map_err(|error| {
            format!("OpenClaw is installed, but the CLI could not be found: {error}")
        })?;
        *self.inner.cli.lock().expect("CLI mutex poisoned") = Some(cli.clone());

        // The installed CLI owns config/state migrations; repair before any
        // Gateway readiness checks consume an outdated home.
        let repair_error = match cli.output(["doctor", "--fix", "--non-interactive"]) {
            Ok(output) if !output.status.success() => Some(
                cli::output_tail(&output.stderr)
                    .unwrap_or_else(|| format!("OpenClaw repair exited with {}", output.status)),
            ),
            Err(error) => Some(format!("OpenClaw repair could not start: {error}")),
            _ => None,
        };
        if let Some(error) = repair_error {
            for line in error.lines() {
                let _ = app.emit_to(
                    "main",
                    "install-progress",
                    serde_json::json!({ "stream": "stderr", "line": line }),
                );
            }
        }

        self.inner
            .navigation
            .lock()
            .map_err(|_| {
                "OpenClaw is installed, but preparing the Gateway dashboard failed: \
                 Dashboard navigation lock is unavailable."
                    .to_string()
            })?
            .mark_onboarding_pending();
        let ready = gateway::ensure_ready(&cli).map_err(|error| {
            format!("OpenClaw is installed, but connecting to the Gateway failed: {error}")
        })?;
        self.finish_local_connection(app, cli, ready)
            .map_err(|error| {
                format!("OpenClaw is installed, but opening the Gateway dashboard failed: {error}")
            })
    }

    pub fn gateway_action(
        &self,
        app: &AppHandle,
        action: GatewayAction,
    ) -> Result<GatewaySnapshot, String> {
        let _operation = self
            .inner
            .operation
            .lock()
            .map_err(|_| "Gateway operation lock is unavailable.".to_string())?;
        if matches!(action, GatewayAction::Stop) {
            self.cancel_watchdog();
        }
        let cli = self.resolve_cli().map_err(|error| error.to_string())?;
        let snapshot = gateway::act(&cli, action)?;
        if matches!(action, GatewayAction::Stop) {
            app.state::<gateway_ws::GatewayClient>()
                .clear_configuration(app);
            self.show_local(app, "stopped", false, None)?;
            self.update_tray(&snapshot);
            return Ok(snapshot);
        }

        let ready = gateway::dashboard(&cli, snapshot)?;
        self.finish_local_connection(app, cli, ready)
    }

    fn finish_local_connection(
        &self,
        app: &AppHandle,
        cli: OpenClawCli,
        ready: ReadyGateway,
    ) -> Result<GatewaySnapshot, String> {
        self.finish_connection(app, Some(cli), ready)
    }

    fn finish_connection(
        &self,
        app: &AppHandle,
        cli: Option<OpenClawCli>,
        ready: ReadyGateway,
    ) -> Result<GatewaySnapshot, String> {
        app.state::<gateway_ws::GatewayClient>()
            .configure(app, ready.gateway_ws);
        let navigated = self.navigate_local(app, &ready.dashboard_url, false, None, true, true)?;
        self.update_tray(&ready.snapshot);
        if navigated {
            self.start_watchdog(app.clone(), cli);
        }
        Ok(ready.snapshot)
    }

    pub fn connect_explicit_local(&self, app: &AppHandle) -> Result<GatewaySnapshot, String> {
        let mut navigation = self
            .inner
            .navigation
            .lock()
            .map_err(|_| "Dashboard navigation lock is unavailable.".to_string())?;
        navigation.permit_local(true, None);
        // First-run setup owns the pending bootstrap reply. Replacing its page
        // drops the error callback and leaves a reconnect screen with no watchdog.
        if !self.main_window_has_local_content(&main_webview(app)?) {
            let mut url = self.inner.local_url.clone();
            url.query_pairs_mut()
                .clear()
                .append_pair("mode", "reconnecting");
            self.navigate_locked(app, url, false)?;
        }
        drop(navigation);
        self.connect_selected(app, true)
    }

    pub(crate) fn connect_remote(
        &self,
        app: &AppHandle,
        request: RemoteGatewayRequest,
    ) -> Result<GatewaySnapshot, String> {
        let _operation = self
            .inner
            .operation
            .lock()
            .map_err(|_| "Gateway operation lock is unavailable.".to_string())?;
        self.connect_remote_locked(app, request)
    }

    fn connect_remote_locked(
        &self,
        app: &AppHandle,
        mut request: RemoteGatewayRequest,
    ) -> Result<GatewaySnapshot, String> {
        remote_gateway::validate_request(&request)?;
        let mut active_tunnel = self
            .inner
            .remote_tunnel
            .lock()
            .map_err(|_| "Remote Gateway tunnel lock is unavailable.".to_string())?;
        active_tunnel.take();
        let (tunnel, gateway_url) = if request.transport == "ssh" {
            let saved_url = request
                .url
                .as_deref()
                .map(remote_gateway::normalize_gateway_url)
                .transpose()?;
            let (tunnel, url) = remote_gateway::start_tunnel(&request, saved_url.as_ref())?;
            (Some(tunnel), url)
        } else {
            let raw = request
                .url
                .as_deref()
                .ok_or_else(|| "Enter the URL of your remote Gateway.".to_string())?;
            (None, remote_gateway::normalize_gateway_url(raw)?)
        };
        remote_gateway::resolve_remote_tls_fingerprint(&mut request, &gateway_url)?;
        let target = remote_gateway::dashboard_url(&gateway_url)?;
        let script = native_auth_initialization_script(&target, &gateway_url, &request)?;
        remote_gateway::save_config_at(&remote_gateway::config_path()?, &request, &gateway_url)?;
        *active_tunnel = tunnel;
        drop(active_tunnel);

        app.state::<gateway_ws::GatewayClient>().configure(
            app,
            gateway_ws::GatewayWsConfig::new(
                gateway_url.to_string(),
                request.token.clone(),
                request.password.clone(),
                if gateway_url.scheme() == "wss" {
                    request.tls_fingerprint.clone()
                } else {
                    None
                },
            ),
        );
        self.navigate_authenticated_remote(app, target, script)?;
        let snapshot = GatewaySnapshot {
            phase: "connected",
            installed: false,
            running: false,
            reachable: true,
            status: "Connected to remote Gateway".to_string(),
            detail: None,
        };
        self.update_tray(&snapshot);
        Ok(snapshot)
    }

    fn navigate_authenticated_remote(
        &self,
        app: &AppHandle,
        dashboard: Url,
        script: String,
    ) -> Result<(), String> {
        let window = app
            .get_window("main")
            .ok_or_else(|| {
                eprintln!("[diag] navigate_authenticated_remote: get_window(main) returned None; windows={:?}",
                    app.webview_windows().keys().collect::<Vec<_>>());
                "Main window is unavailable.".to_string()
            })?;
        let size = window
            .inner_size()
            .map_err(|_| "Could not measure the Gateway window.".to_string())?;
        let mut navigation = self
            .inner
            .navigation
            .lock()
            .map_err(|_| "Dashboard navigation lock is unavailable.".to_string())?;
        let old_webview = app
            .get_webview("main")
            .ok_or_else(|| "Main dashboard view is unavailable.".to_string())?;
        navigation.select_remote();
        // Keep the native window alive: only its child changes so tray ownership,
        // geometry and close-to-tray behavior survive auth-bound script injection.
        old_webview
            .close()
            .map_err(|_| "Could not replace the Gateway dashboard view.".to_string())?;
        let browser_app = app.clone();
        let mut builder = WebviewBuilder::new("main", WebviewUrl::External(dashboard))
            .initialization_script(script)
            .initialization_script(parity_init_script())
            .on_new_window(move |url, _features| {
                open_external_browser(&browser_app, &url);
                NewWindowResponse::Deny
            })
            .auto_resize();
        if let Some(args) = webview_debug_browser_args() {
            builder = builder.additional_browser_args(args.as_str());
        }
        if window
            .add_child(builder, LogicalPosition::new(0, 0), size)
            .is_err()
        {
            navigation.remote_dashboard = false;
            let browser_app = app.clone();
            let mut restore = WebviewBuilder::new("main", WebviewUrl::App("index.html".into()))
                .initialization_script(parity_init_script())
                .on_new_window(move |url, _features| {
                    open_external_browser(&browser_app, &url);
                    NewWindowResponse::Deny
                })
                .auto_resize();
            if let Some(args) = webview_debug_browser_args() {
                restore = restore.additional_browser_args(args.as_str());
            }
            let _ = window.add_child(restore, LogicalPosition::new(0, 0), size);
            native_browser::install(app.clone());
            return Err(
                "Could not open the remote Gateway dashboard. Try connecting again.".to_string(),
            );
        }
        drop(navigation);
        native_browser::install(app.clone());
        tray::show_window(app);
        Ok(())
    }

    pub fn show_error(&self, app: &AppHandle, _error: &str) {
        let _ = self.show_local(app, "error", false, None);
        self.update_tray(&GatewaySnapshot::reconnecting("Gateway action failed."));
        tray::show_window(app);
    }

    pub fn quit(&self) {
        self.inner.quitting.store(true, Ordering::SeqCst);
        self.cancel_watchdog();
        if let Ok(mut tunnel) = self.inner.remote_tunnel.lock() {
            tunnel.take();
        }
    }

    fn is_quitting(&self) -> bool {
        self.inner.quitting.load(Ordering::SeqCst)
    }

    pub(crate) fn resolve_cli(&self) -> Result<OpenClawCli, CliError> {
        if let Some(cli) = self
            .inner
            .cli
            .lock()
            .expect("CLI mutex poisoned")
            .clone()
            .filter(OpenClawCli::is_available)
        {
            return Ok(cli);
        }
        let cli = OpenClawCli::discover()?;
        *self.inner.cli.lock().expect("CLI mutex poisoned") = Some(cli.clone());
        Ok(cli)
    }

    pub(crate) fn main_window_has_local_content(&self, webview: &Webview) -> bool {
        webview.url().is_ok_and(|mut current_url| {
            let mut local_url = self.inner.local_url.clone();
            current_url.set_query(None);
            current_url.set_fragment(None);
            local_url.set_query(None);
            local_url.set_fragment(None);
            current_url == local_url
        })
    }

    fn update_tray(&self, snapshot: &GatewaySnapshot) {
        if let Some(tray) = self
            .inner
            .tray
            .lock()
            .expect("tray mutex poisoned")
            .as_ref()
        {
            tray.update(snapshot);
        }
    }

    fn show_missing_cli(
        &self,
        app: &AppHandle,
        force: bool,
        expected_generation: Option<u64>,
    ) -> Result<GatewaySnapshot, String> {
        let snapshot = GatewaySnapshot::missing_cli();
        let navigation = self.show_local(app, "missingCli", force, expected_generation);
        if !local_recovery_owns_gateway(&navigation) {
            return Ok(snapshot);
        }
        app.state::<gateway_ws::GatewayClient>()
            .clear_configuration(app);
        self.update_tray(&snapshot);
        navigation.map(|_| snapshot)
    }

    fn show_cli_recovery_error(&self, app: &AppHandle, generation: u64, error: CliError) {
        let mut snapshot = GatewaySnapshot::missing_cli();
        snapshot.status = "CLI unavailable".to_string();
        snapshot.detail = Some(error.to_string());
        let navigation = self.show_local(app, "error", false, Some(generation));
        if local_recovery_owns_gateway(&navigation) {
            app.state::<gateway_ws::GatewayClient>()
                .clear_configuration(app);
            self.update_tray(&snapshot);
        }
    }

    /// Approval polling costs two CLI subprocesses per round, so it runs off the
    /// watchdog thread, on its own cadence, and never overlaps itself.
    fn spawn_approval_poll(&self, app: &AppHandle, generation: u64, tick: u64) {
        // Each round boots the Node CLI twice. While the window is focused the
        // dashboard already shows pending approvals, so skip the poll entirely
        // and leave the Gateway alone.
        if main_window_handle(app).is_ok_and(|window| matches!(window.is_focused(), Ok(true))) {
            return;
        }
        if tick % APPROVAL_POLL_TICKS != 0
            || self.inner.approvals_polling.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let app = app.clone();
        let state = self.clone();
        thread::spawn(move || {
            if let Ok(cli) = state.resolve_cli() {
                state.poll_pending_approvals(&app, &cli, generation);
            }
            state.inner.approvals_polling.store(false, Ordering::Release);
        });
    }

    fn poll_pending_approvals(&self, app: &AppHandle, cli: &OpenClawCli, generation: u64) {
        let pending = match pending_approvals::fetch(cli) {
            Ok(pending) => pending,
            Err(error) => {
                eprintln!("Could not poll pending approvals: {error}");
                return;
            }
        };
        if !self.watchdog_is_current(generation) {
            return;
        }
        let diff = self
            .inner
            .pending_approvals
            .lock()
            .expect("pending approval mutex poisoned")
            .update(pending);
        if let Some(tray) = self
            .inner
            .tray
            .lock()
            .expect("tray mutex poisoned")
            .as_ref()
        {
            tray.update_pending_count(diff.count);
        }
        if !main_window_handle(app).is_ok_and(|window| matches!(window.is_focused(), Ok(false))) {
            return;
        }
        // Notifications are a doorbell only; approval stays in the dashboard or CLI.
        for request in diff.new {
            notify::notify(app, "OpenClaw", &request.notification_body());
        }
    }

    // Caller holds the navigation lock, keeping the final arbitration check and navigation atomic.
    fn navigate_locked(
        &self,
        app: &AppHandle,
        url: Url,
        reveal_window: bool,
    ) -> Result<(), String> {
        main_webview(app)?
            .navigate(url)
            .map_err(|error| format!("Could not open dashboard: {error}"))?;
        if reveal_window {
            tray::show_window(app);
        }
        Ok(())
    }

    fn navigate_local(
        &self,
        app: &AppHandle,
        target: &str,
        force: bool,
        expected_generation: Option<u64>,
        reveal_window: bool,
        dashboard: bool,
    ) -> Result<bool, String> {
        let mut navigation = self
            .inner
            .navigation
            .lock()
            .map_err(|_| "Dashboard navigation lock is unavailable.".to_string())?;
        if !navigation.permit_local(force, expected_generation) {
            return Ok(false);
        }
        let onboarding_was_pending = dashboard && navigation.onboarding_pending;
        let url = if dashboard {
            navigation.prepare_dashboard_url(target)?
        } else {
            Url::parse(target).map_err(|_| "Dashboard returned an invalid URL.".to_string())?
        };
        if let Err(error) = self.navigate_locked(app, url, reveal_window) {
            if onboarding_was_pending {
                navigation.mark_onboarding_pending();
            }
            return Err(error);
        }
        Ok(true)
    }

    fn show_local(
        &self,
        app: &AppHandle,
        mode: &str,
        force: bool,
        expected_generation: Option<u64>,
    ) -> Result<bool, String> {
        let mut url = self.inner.local_url.clone();
        url.query_pairs_mut().clear().append_pair("mode", mode);
        // Status/watchdog updates may change the hidden WebView, but must not reveal it.
        self.navigate_local(app, url.as_str(), force, expected_generation, false, false)
    }

    fn cancel_watchdog(&self) {
        if let Ok(mut navigation) = self.inner.navigation.lock() {
            navigation.cancel_watchdog();
        }
    }

    fn watchdog_is_current(&self, generation: u64) -> bool {
        self.inner
            .navigation
            .lock()
            .is_ok_and(|navigation| navigation.watchdog_is_current(generation))
    }

    fn start_watchdog(&self, app: AppHandle, cli: Option<OpenClawCli>) {
        let generation = {
            let Ok(mut navigation) = self.inner.navigation.lock() else {
                return;
            };
            let Some(generation) = navigation.begin_watchdog() else {
                return;
            };
            generation
        };
        let state = self.clone();
        thread::spawn(move || {
            let mut tick: u64 = 0;
            let mut cli = cli;
            let mut probe_failures: u32 = 0;
            let mut cli_attempts: u32 = 0;
            let mut next_cli_check: Option<Instant> = None;
            loop {
                thread::sleep(CONNECTED_WATCH_INTERVAL);
                tick = tick.wrapping_add(1);
                if !state.watchdog_is_current(generation) {
                    return;
                }
                let Ok(_operation) = state.inner.operation.try_lock() else {
                    continue;
                };

                // The connected tick must stay cheap. `gateway status --json`
                // boots the whole Node CLI (20s+ on a large plugin set), so the
                // watchdog answers "is it up?" with one loopback request and
                // only shells out when that probe says the Gateway is down.
                let probe_state = gateway::local_gateway_probe()
                    .ok()
                    .flatten()
                    .map_or(gateway::LoopbackState::Down, |probe| probe.state());
                if probe_state.is_reachable() {
                    probe_failures = 0;
                    cli_attempts = 0;
                    next_cli_check = None;
                    state.update_tray(&GatewaySnapshot::connected());
                    drop(_operation);
                    state.spawn_approval_poll(&app, generation, tick);
                    continue;
                }

                // A port that accepts connections but never answers means the
                // Gateway is alive and saturated, not offline. Keep the
                // dashboard and stay off the CLI: booting Node here is what
                // turned a slow Gateway into a white-screened client (probe
                // fails -> CLI storm -> event loop saturates further -> probe
                // fails harder).
                probe_failures = probe_failures.saturating_add(1);
                let stalled = matches!(probe_state, gateway::LoopbackState::Stalled);
                if stalled {
                    state.update_tray(&GatewaySnapshot::busy(
                        "Gateway is responding slowly; the shell is waiting instead of restarting it.",
                    ));
                }
                let now = Instant::now();
                let cli_due = next_cli_check.is_none_or(|at| now >= at);
                if (stalled && probe_failures < STALLS_BEFORE_CLI) || !cli_due {
                    continue;
                }
                next_cli_check = Some(now + cli_backoff(cli_attempts));
                cli_attempts = cli_attempts.saturating_add(1);

                if cli.as_ref().is_none_or(|existing| !existing.is_available()) {
                    match state.resolve_cli() {
                        Ok(discovered) => cli = Some(discovered),
                        Err(error) => {
                            if matches!(error, CliError::Missing) {
                                let _ = state.show_missing_cli(&app, false, Some(generation));
                            } else {
                                state.show_cli_recovery_error(&app, generation, error);
                            }
                            return;
                        }
                    }
                }
                let Some(active_cli) = cli.clone() else {
                    return;
                };
                let snapshot = match gateway::status(&active_cli) {
                    Ok(snapshot) => snapshot,
                    Err(error) => GatewaySnapshot::reconnecting(error),
                };
                if snapshot.reachable {
                    // Transient probe failure: the CLI confirms the Gateway is
                    // up, so keep the dashboard and stay on the cheap path.
                    probe_failures = 0;
                    cli_attempts = 0;
                    next_cli_check = None;
                    state.update_tray(&snapshot);
                    drop(_operation);
                    state.spawn_approval_poll(&app, generation, tick);
                    continue;
                }

                // Onboarding keeps verification and guided-session state in its live page. Latch it
                // for this outage so neither recovery screen nor dashboard reload erases that state.
                let preserve_dashboard = main_webview(&app)
                    .ok()
                    .and_then(|webview| webview.url().ok())
                    .is_some_and(|url| is_active_onboarding_url(&url));
                let mut displayed_phase = snapshot.phase;
                if !preserve_dashboard
                    && matches!(
                        state.show_local(&app, local_mode(&snapshot), false, Some(generation)),
                        Ok(false)
                    )
                {
                    return;
                }
                state.update_tray(&snapshot);
                drop(_operation);
                let mut recovery_cli = active_cli;
                loop {
                    if !state.watchdog_is_current(generation) {
                        return;
                    }
                    if let Ok(_operation) = state.inner.operation.try_lock() {
                        if !recovery_cli.is_available() {
                            match state.resolve_cli() {
                                Ok(discovered) => recovery_cli = discovered,
                                Err(error) => {
                                    if matches!(error, CliError::Missing) {
                                        let _ =
                                            state.show_missing_cli(&app, false, Some(generation));
                                    } else {
                                        state.show_cli_recovery_error(&app, generation, error);
                                    }
                                    return;
                                }
                            }
                        }
                        let snapshot = match gateway::status(&recovery_cli) {
                            Ok(snapshot) => snapshot,
                            Err(error) => GatewaySnapshot::reconnecting(error),
                        };
                        state.update_tray(&snapshot);
                        if snapshot.reachable {
                            if let Ok(ready) = gateway::dashboard(&recovery_cli, snapshot) {
                                app.state::<gateway_ws::GatewayClient>()
                                    .configure(&app, ready.gateway_ws.clone());
                                if preserve_dashboard {
                                    state.update_tray(&ready.snapshot);
                                    break;
                                }
                                match state.navigate_local(
                                    &app,
                                    &ready.dashboard_url,
                                    false,
                                    Some(generation),
                                    false,
                                    true,
                                ) {
                                    Ok(true) => {
                                        state.update_tray(&ready.snapshot);
                                        break;
                                    }
                                    Ok(false) => return,
                                    Err(_) => {}
                                }
                            }
                        } else if !preserve_dashboard && snapshot.phase != displayed_phase {
                            displayed_phase = snapshot.phase;
                            if matches!(
                                state.show_local(
                                    &app,
                                    local_mode(&snapshot),
                                    false,
                                    Some(generation),
                                ),
                                Ok(false)
                            ) {
                                return;
                            }
                        }
                    }
                    thread::sleep(RECONNECT_INTERVAL);
                }
            }
        });
    }
}

fn local_mode(snapshot: &GatewaySnapshot) -> &'static str {
    if snapshot.installed && !snapshot.running {
        "stopped"
    } else {
        "reconnecting"
    }
}

fn local_recovery_owns_gateway(navigation: &Result<bool, String>) -> bool {
    !matches!(navigation, Ok(false))
}

#[cfg(test)]
mod navigation_tests {
    use super::{
        is_active_onboarding_url, is_release_version, local_recovery_owns_gateway, NavigationState,
        Url,
    };

    #[test]
    fn only_active_onboarding_preserves_the_dashboard_during_reconnect() {
        for (url, preserve) in [
            ("http://127.0.0.1/settings/model-setup?firstRun=1", true),
            (
                "http://127.0.0.1/settings/model-setup?firstRun=explicit",
                true,
            ),
            (
                "http://127.0.0.1/openclaw/settings/model-setup/?tab=ai&firstRun=1#token=redacted",
                true,
            ),
            ("http://127.0.0.1/settings/model-setup", false),
            ("http://127.0.0.1/settings/model-setup?firstRun=0", false),
            (
                "http://127.0.0.1/settings/model-setup?firstRun=0&firstRun=1",
                false,
            ),
            ("http://127.0.0.1/settings/providers?firstRun=1", false),
            ("http://127.0.0.1/custodian?onboarding=1", true),
            (
                "http://127.0.0.1/openclaw/custodian/?tab=chat&onboarding=YES",
                true,
            ),
            ("http://127.0.0.1/custodian", false),
            ("http://127.0.0.1/custodian?onboarding=0", false),
            (
                "http://127.0.0.1/custodian?onboarding=0&onboarding=1",
                false,
            ),
            ("http://127.0.0.1/chat?onboarding=1", false),
        ] {
            assert_eq!(
                is_active_onboarding_url(&Url::parse(url).expect("dashboard URL")),
                preserve,
                "unexpected reconnect policy for {url}"
            );
        }
    }

    #[test]
    fn committed_package_version_is_a_development_build() {
        assert!(!is_release_version("0.1.0"));
    }

    #[test]
    fn stamped_package_versions_are_release_builds() {
        assert!(is_release_version("2026.7.2"));
        assert!(is_release_version("2026.7.2-beta.1"));
    }

    #[test]
    fn newer_remote_selection_blocks_older_local_navigation() {
        let mut navigation = NavigationState::default();
        assert!(navigation.permit_local(false, None));

        navigation.select_remote();

        assert!(!navigation.permit_local(false, None));
        assert!(navigation.remote_dashboard);
    }

    #[test]
    fn newer_remote_selection_invalidates_watchdog_navigation() {
        let mut navigation = NavigationState::default();
        let watchdog = navigation.begin_watchdog().expect("watchdog generation");

        navigation.select_remote();

        assert!(!navigation.permit_local(false, Some(watchdog)));
        assert!(!navigation.watchdog_is_current(watchdog));
    }

    #[test]
    fn explicit_local_then_later_remote_preserves_latest_intent() {
        let mut navigation = NavigationState::default();
        navigation.select_remote();
        assert!(navigation.permit_local(true, None));
        assert!(!navigation.remote_dashboard);

        navigation.select_remote();

        assert!(!navigation.permit_local(false, None));
        assert!(navigation.remote_dashboard);
    }

    #[test]
    fn local_recovery_clears_retained_gateway_unless_remote_navigation_won() {
        assert!(local_recovery_owns_gateway(&Ok(true)));
        assert!(local_recovery_owns_gateway(&Err(
            "local navigation failed".to_string()
        )));
        assert!(!local_recovery_owns_gateway(&Ok(false)));
    }

    #[test]
    fn first_run_url_preserves_gateway_base_path_query_and_auth_fragment() {
        let mut navigation = NavigationState::default();
        navigation.mark_onboarding_pending();

        let url = navigation
            .prepare_dashboard_url(
                "http://127.0.0.1:18789/openclaw/?foo=bar&firstRun=1#token=secret",
            )
            .expect("dashboard URL");

        assert_eq!(url.path(), "/openclaw/settings/model-setup");
        assert_eq!(url.query(), Some("foo=bar&firstRun=explicit"));
        assert_eq!(url.fragment(), Some("token=secret"));
    }

    #[test]
    fn first_run_model_setup_is_opened_only_once() {
        let mut navigation = NavigationState::default();
        navigation.mark_onboarding_pending();

        let first = navigation
            .prepare_dashboard_url("http://127.0.0.1:18789/#token=secret")
            .expect("first dashboard URL");
        let second = navigation
            .prepare_dashboard_url("http://127.0.0.1:18789/#token=secret")
            .expect("second dashboard URL");

        assert_eq!(first.path(), "/settings/model-setup");
        assert_eq!(first.query(), Some("firstRun=explicit"));
        assert!(is_active_onboarding_url(&first));
        assert_eq!(second.path(), "/");
        assert_eq!(second.query(), None);
        assert!(!is_active_onboarding_url(&second));
    }

    #[test]
    fn regular_navigation_has_no_onboarding_marker() {
        let mut navigation = NavigationState::default();

        let url = navigation
            .prepare_dashboard_url("http://127.0.0.1:18789/?foo=bar#token=secret")
            .expect("dashboard URL");

        assert_eq!(url.query(), Some("foo=bar"));
        assert_eq!(url.fragment(), Some("token=secret"));
    }
}

/// The shell window that hosts the dashboard.
///
/// Starship embeds browser tabs as child webviews of this window, and
/// `AppHandle::get_webview_window("main")` only resolves while every webview in
/// that window carries the window label. As soon as one `native-browser-*` tab
/// exists it returns `None`, which used to fail bootstrap, tray reveal and every
/// watchdog repaint with "Main window is unavailable.". Resolve the window and
/// its dashboard webview separately instead of relying on that shortcut.
/// Diagnostic trail for "the client disappeared" reports.
///
/// The shell has exactly one self-initiated exit - the tray Quit item - so when
/// a user reports the window vanishing alongside another app, the only way to
/// separate "we exited" from "something terminated us" is a witness outside the
/// process. `shell-lifecycle.log` records every exit-ish event, and
/// `shell-heartbeat.json` is rewritten every few seconds so a hard kill still
/// leaves a usable "last seen alive" timestamp behind.
pub(crate) mod shell_lifecycle {
    use std::io::Write;
    use std::path::PathBuf;

    fn state_dir() -> Option<PathBuf> {
        let base = std::env::var("LOCALAPPDATA").ok()?;
        Some(PathBuf::from(base).join("ai.starship.client"))
    }

    fn stamp() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        format!("{}.{:03}", now.as_secs(), now.subsec_millis())
    }

    pub(crate) fn event(message: &str) {
        let Some(dir) = state_dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("shell-lifecycle.log"))
        {
            let _ = writeln!(file, "{} pid={} {message}", stamp(), std::process::id());
        }
    }

    fn beat() {
        let Some(dir) = state_dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join("shell-heartbeat.json"),
            format!(
                "{{\"pid\":{},\"at\":\"{}\"}}\n",
                std::process::id(),
                stamp()
            ),
        );
    }

    fn start_heartbeat() {
        std::thread::spawn(|| loop {
            beat();
            std::thread::sleep(std::time::Duration::from_secs(5));
        });
    }

    /// Records the launch and starts the liveness witness.
    pub(crate) fn install() {
        event("shell-start");
        start_heartbeat();
    }
}

/// Crash forensics for the "the client disappears and there is no log" reports.
///
/// WebView2 hands control back to us through `extern "system"` COM callbacks.
/// A panic raised inside one of those callbacks cannot unwind across the FFI
/// boundary, so Rust fast-fails the process with `0xc0000409` before the
/// default hook output (which goes to a stderr this GUI process does not own)
/// reaches anyone. `panic.log` keeps the message, source location and backtrace
/// of the *next* abort, and [`guard`] lets every callback we own swallow a
/// panic instead of aborting the client.
pub(crate) mod crash_log {
    use std::io::Write;
    use std::path::PathBuf;

    fn state_dir() -> Option<PathBuf> {
        let base = std::env::var("LOCALAPPDATA").ok()?;
        Some(PathBuf::from(base).join("ai.starship.client"))
    }

    fn stamp() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        format!("{}.{:03}", now.as_secs(), now.subsec_millis())
    }

    pub(crate) fn record(message: &str) {
        let Some(dir) = state_dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("panic.log"))
        {
            let _ = writeln!(
                file,
                "{} pid={} {message}",
                stamp(),
                std::process::id()
            );
        }
    }

    fn describe(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(message) = payload.downcast_ref::<&str>() {
            (*message).to_string()
        } else if let Some(message) = payload.downcast_ref::<String>() {
            message.clone()
        } else {
            "<non-string panic payload>".to_string()
        }
    }

    /// Runs `body`, turning a panic into a logged `None`.
    ///
    /// Use this around the body of every WebView2/COM callback: those run on a
    /// stack the runtime entered through `extern "system"`, where unwinding is
    /// not allowed and any escaping panic aborts the whole client.
    pub(crate) fn guard<T>(label: &str, body: impl FnOnce() -> T) -> Option<T> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            Ok(value) => Some(value),
            Err(payload) => {
                record(&format!(
                    "suppressed panic in {label}: {}",
                    describe(payload.as_ref())
                ));
                None
            }
        }
    }

    /// Installs the logging hook. Call once, before anything else can panic.
    pub(crate) fn install() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|location| {
                    format!(
                        "{}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                })
                .unwrap_or_else(|| "<unknown location>".to_string());
            let backtrace = std::backtrace::Backtrace::force_capture();
            record(&format!(
                "panic at {location}: {}\n{backtrace}",
                describe(info.payload())
            ));
            previous(info);
        }));
    }
}

fn main_window_handle(app: &AppHandle) -> Result<Window, String> {
    app.get_window("main")
        .ok_or_else(|| "Main window is unavailable.".to_string())
}

/// The dashboard webview hosted by [`main_window_handle`].
fn main_webview(app: &AppHandle) -> Result<Webview, String> {
    app.get_webview("main")
        .ok_or_else(|| "Main dashboard view is unavailable.".to_string())
}

#[tauri::command]
fn build_info(app: AppHandle) -> BuildInfo {
    let version = app.package_info().version.to_string();
    BuildInfo {
        release_build: is_release_version(&version),
        version,
    }
}

#[tauri::command]
async fn bootstrap(
    operations: State<'_, GatewayOperationQueue>,
    explicit_local: Option<bool>,
) -> Result<GatewaySnapshot, String> {
    let operation = if explicit_local == Some(true) {
        GatewayOperation::ConnectExplicitLocal
    } else {
        GatewayOperation::Connect
    };
    operations.execute(operation).await
}

#[tauri::command]
async fn connect_remote_gateway(
    operations: State<'_, GatewayOperationQueue>,
    transport: String,
    url: Option<String>,
    ssh_target: Option<String>,
    token: Option<String>,
    password: Option<String>,
    remote_port: Option<u16>,
) -> Result<GatewaySnapshot, String> {
    operations
        .execute(GatewayOperation::ConnectRemote(RemoteGatewayRequest {
            transport,
            url,
            ssh_target,
            token,
            password,
            remote_port,
            tls_fingerprint: None,
        }))
        .await
}

#[tauri::command]
async fn install_cli(
    operations: State<'_, GatewayOperationQueue>,
    channel: InstallChannel,
) -> Result<GatewaySnapshot, String> {
    operations.execute(GatewayOperation::Install(channel)).await
}

#[tauri::command]
async fn gateway_action(
    operations: State<'_, GatewayOperationQueue>,
    action: GatewayAction,
) -> Result<GatewaySnapshot, String> {
    operations.execute(GatewayOperation::Action(action)).await
}

fn main() {
    // Must run before anything else observes the process: a job-scoped launcher
    // would otherwise take the client down with it (see `windows_job`).
    crash_log::install();
    windows_job::ensure_outside_job();
    shell_lifecycle::install();
    let global_shortcuts_supported = tray::global_shortcuts_supported();
    let quickchat_state = quickchat::QuickChatState::new(global_shortcuts_supported);
    let quickchat_shortcut_state = quickchat_state.clone();
    // Single-instance must run first so it can pass deep-link argv to the primary process.
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_window(app);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ));
    // global-hotkey's Linux backend is X11-only; omit it on Wayland instead of using XWayland.
    // A GlobalShortcuts portal can follow later.
    let builder = if global_shortcuts_supported {
        builder.plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        if quickchat_shortcut_state.matches_shortcut(shortcut) {
                            quickchat::toggle_quickchat(app);
                        } else if shortcut
                            .matches(Modifiers::CONTROL | Modifiers::SHIFT, Code::KeyO)
                        {
                            tray::show_window(app);
                        }
                    }
                })
                .build(),
        )
    } else {
        builder
    };
    let builder = notify::register(builder)
        .plugin(
            tauri_plugin_opener::Builder::new()
                // Dashboard links use the native handler; its renderer has no opener IPC grant.
                .open_js_links_on_click(false)
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_denylist(&[quickchat::QUICKCHAT_LABEL])
                .build(),
        );

    let builder = builder.setup(move |app| {
        let window_config = app
            .config()
            .app
            .windows
            .iter()
            .find(|window| window.label == "main")
            .cloned()
            .expect("tauri.conf.json must define the main window");
        let browser_app = app.handle().clone();
        // 首屏窗口来自 tauri.conf.json，不经过上面那两处 builder；诊断端口要在这里
        // 也接一次，否则 STARSHIP_WEBVIEW_DEBUG_PORT 只在重新导航后才生效。
        let mut main_window = WebviewWindowBuilder::from_config(app.handle(), &window_config)?
            .initialization_script(parity_init_script())
            .on_new_window(move |url, _features| {
                open_external_browser(&browser_app, &url);
                NewWindowResponse::Deny
            });
        if let Some(args) = webview_debug_browser_args() {
            main_window = main_window.additional_browser_args(args.as_str());
        }
        let window = main_window.build()?;
        let state = DesktopState::new(window.url()?);
        app.manage(state.clone());
        app.manage(gateway_ws::GatewayClient::new());
        #[cfg(target_os = "linux")]
        app.manage(gateway_sleep_logind::SleepBridge::start(
            app.handle().clone(),
        ));
        let operation_app = app.handle().clone();
        let operation_state = state.clone();
        let error_app = app.handle().clone();
        let error_state = state.clone();
        // Every caller of the operation mutex enters this queue so UI source cannot reorder work.
        app.manage(GatewayOperationQueue::new(
            move |operation| match operation {
                GatewayOperation::Connect => operation_state.connect(&operation_app),
                GatewayOperation::ConnectExplicitLocal => {
                    operation_state.connect_explicit_local(&operation_app)
                }
                GatewayOperation::ConnectRemote(request) => {
                    operation_state.connect_remote(&operation_app, request)
                }
                GatewayOperation::Install(channel) => {
                    operation_state.install_cli(&operation_app, channel)
                }
                GatewayOperation::Action(action) => {
                    operation_state.gateway_action(&operation_app, action)
                }
            },
            move |error| error_state.show_error(&error_app, error),
        ));
        let deep_link_app = app.handle().clone();
        app.deep_link().on_open_url(move |event| {
            handle_deep_links(&deep_link_app, event.urls());
        });
        if let Some(urls) = app.deep_link().get_current()? {
            handle_deep_links(app.handle(), urls);
        }
        #[cfg(any(target_os = "linux", all(debug_assertions, target_os = "windows")))]
        if let Err(error) = app.deep_link().register_all() {
            eprintln!("Deep-link registration unavailable: {error}");
        }

        app.manage(discovery::GatewayDiscovery::default());
        app.manage(quickchat_state.clone());
        app.manage(updater::UpdaterState::default());
        app.manage(native_browser::NativeBrowserState::default());
        native_browser::install(app.handle().clone());
        state.set_tray(tray::build(app, state.clone(), global_shortcuts_supported)?);
        Ok(())
    });
    let builder = builder.invoke_handler(tauri::generate_handler![
        bootstrap,
        build_info,
        updater::check_for_updates,
        discovery::connect_discovered_gateway,
        connect_remote_gateway,
        discovery::discover_gateways,
        install_cli,
        gateway_action,
        quickchat::quickchat_activate,
        quickchat::quickchat_agents,
        quickchat::quickchat_hide,
        quickchat::quickchat_identity,
        quickchat::quickchat_ready,
        quickchat::quickchat_select_agent,
        quickchat::quickchat_send,
        quickchat::quickchat_set_expanded,
        quickchat::quickchat_set_shortcut,
        quickchat::quickchat_shortcut,
        quickchat::quickchat_show_dashboard,
        quickchat_widgets::quickchat_refresh_widget_surface,
        quickchat_widgets::quickchat_sync_widgets,
        updater::open_release_page,
        updater::relaunch,
        updater::updater_ready
    ]);

    let app = builder
        .on_window_event(|window, event| {
            if window.label() == quickchat::QUICKCHAT_LABEL {
                match event {
                    tauri::WindowEvent::Focused(false) => {
                        // GTK queues focus events; a stale blur must not hide a refocused window.
                        if cfg!(target_os = "linux") && window.is_focused().unwrap_or(false) {
                            return;
                        }
                        quickchat::request_hide(window.app_handle());
                        return;
                    }
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        quickchat::request_hide(window.app_handle());
                        return;
                    }
                    _ => {}
                }
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label().starts_with("gateway-") {
                    return;
                }
                let state = window.app_handle().state::<DesktopState>();
                if !state.is_quitting() {
                    api.prevent_close();
                    shell_lifecycle::event("window-close-requested");
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("OpenClaw desktop app failed");
    app.run(|app, event| {
        #[cfg(target_os = "linux")]
        if matches!(event, tauri::RunEvent::Exit) {
            if let Some(bridge) = app.try_state::<gateway_sleep_logind::SleepBridge>() {
                bridge.shutdown();
            }
            if let Some(state) = app.try_state::<DesktopState>() {
                state.quit();
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            if matches!(event, tauri::RunEvent::Exit) {
                shell_lifecycle::event("run-exit");
            }
            let _ = (app, event);
        }
    });
}
