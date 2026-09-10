//! Starship shell-only native browser panel.
//!
//! The official OpenClaw dashboard owns the browser panel UI and talks to the
//! desktop shell through `window.webkit.messageHandlers.openclawBrowser`
//! (the macOS host protocol). Windows has no WebKit message handler, so this
//! module:
//!
//! 1. injects a shim that exposes that protocol on top of the WebView2 web
//!    message channel (`window.chrome.webview`), and
//! 2. implements the host side with real WebView2 child views that are
//!    positioned over the dashboard's `.bp-stage` rectangle.
//!
//! Nothing here patches the official UI bundle: the shim only makes
//! `hasNativeBrowserBridge()` report `true`, which is the documented
//! extension point the official dashboard already ships.

#[cfg(target_os = "windows")]
mod windows_impl {
    use serde_json::{json, Value};
    use std::collections::{HashMap, HashSet};
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};
    use tauri::webview::{NewWindowResponse, WebviewBuilder};
    use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, Url, Webview, WebviewUrl};
    use webview2_com::{
        take_pwstr, CallDevToolsProtocolMethodCompletedHandler, ContentLoadingEventHandler,
        DocumentTitleChangedEventHandler, ExecuteScriptCompletedHandler,
        HistoryChangedEventHandler, NavigationCompletedEventHandler,
        NavigationStartingEventHandler, NewWindowRequestedEventHandler,
        ProcessFailedEventHandler, SourceChangedEventHandler, WebMessageReceivedEventHandler,
    };
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
    use windows::core::{HSTRING, BOOL, PWSTR};

    /// Script evaluated in the dashboard before any of its own scripts run.
    pub const INIT_SCRIPT: &str = r#"
(function () {
  if (window.__starshipNativeBrowserInstalled) { return; }
  if (!window.chrome || !window.chrome.webview) { return; }
  window.__starshipNativeBrowserInstalled = true;
  var pending = new Map();
  var sequence = 0;
  function expire(id) {
    var resolve = pending.get(id);
    if (!resolve) { return; }
    pending.delete(id);
    resolve({ ok: false, error: "Starship native browser request timed out" });
  }
  function postMessage(message) {
    return new Promise(function (resolve) {
      var id = "starship-" + Date.now().toString(36) + "-" + (++sequence).toString(36);
      pending.set(id, resolve);
      try {
        // WebView2 only delivers string payloads to WebMessageReceived in this
        // host (object payloads are silently dropped), so serialize explicitly.
        window.chrome.webview.postMessage(
          JSON.stringify({ __starship: true, id: id, message: message }),
        );
      } catch (error) {
        pending.delete(id);
        resolve({ ok: false, error: "Starship native browser bridge is unavailable" });
        return;
      }
      window.setTimeout(function () { expire(id); }, 20000);
    });
  }
  window.chrome.webview.addEventListener("message", function (event) {
    var data = event.data;
    if (!data || typeof data !== "object") { return; }
    if (data.__starshipReply === true && typeof data.id === "string") {
      var resolve = pending.get(data.id);
      if (resolve) {
        pending.delete(data.id);
        resolve(data.reply);
      }
      return;
    }
    if (data.__starshipState === true && data.state) {
      window.__OPENCLAW_NATIVE_BROWSER__ = data.state;
      try {
        window.dispatchEvent(
          new CustomEvent("openclaw:native-browser-state", { detail: data.state }),
        );
      } catch (error) {
        /* CustomEvent is available in every WebView2 runtime we support. */
      }
    }
  });
  var host = window.webkit || (window.webkit = {});
  var handlers = host.messageHandlers || (host.messageHandlers = {});
  handlers.openclawBrowser = { postMessage: postMessage };
  if (!window.__OPENCLAW_NATIVE_BROWSER__) {
    window.__OPENCLAW_NATIVE_BROWSER__ = { revision: 0, tabs: [] };
  }
  // The official dashboard only presents the embedded browser while the main
  // chat pane owns input. Starship's assistant dock can claim input ownership,
  // which leaves the native child view hidden behind a visibly open panel.
  // Report the real stage geometry so the shell can keep the child WebView2
  // aligned without patching the official UI bundle.
  var lastShellProbe = "";
  // The shell navigates the main view (local bootstrap -> gateway dashboard)
  // after the first session restore, so every document needs its own handshake.
  // Without it a freshly loaded dashboard keeps an empty
  // `window.__OPENCLAW_NATIVE_BROWSER__` and the panel renders blank even
  // though the tab webviews are alive.
  var shellDocumentId =
    "doc-" + Date.now().toString(36) + "-" + Math.random().toString(36).slice(2);
  // The dashboard keeps every visited chat pane mounted (`chat-pane-cache__pane`)
  // and each of them owns an `openclaw-browser-panel`. Measuring the first panel
  // in document order can therefore report a cached pane that the user is not
  // looking at, and the child WebView2 lands on a hidden slot. Measure the pane
  // that is actually on screen instead.
  var LIVE_PANE_CLASS = "chat-pane-cache__pane--visible";
  var ACTIVE_PANE_CLASS = "chat-pane-cache__pane--active";
  function livePanelMeasurement() {
    var panels = document.querySelectorAll("openclaw-browser-panel");
    var best = null;
    for (var index = 0; index < panels.length; index += 1) {
      var panel = panels[index];
      if (!panel.shadowRoot) { continue; }
      var pane = typeof panel.closest === "function"
        ? panel.closest("openclaw-chat-pane")
        : null;
      if (pane && !pane.classList.contains(LIVE_PANE_CLASS)) { continue; }
      var panelVisible =
        typeof panel.checkVisibility === "function"
          ? panel.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })
          : panel.offsetParent !== null;
      if (!panelVisible) { continue; }
      var stage = panel.shadowRoot.querySelector(".bp-stage");
      var controller = panel.browserPanelController;
      var tabId = controller && controller.activeTargetId;
      if (!stage || typeof tabId !== "string" || !tabId) { continue; }
      // Every panel instance owns a stable presentation scope. A tab id alone
      // cannot tell "the pane on screen is presenting" apart from "a cached
      // pane is presenting", because right after the panel swaps tabs its
      // active tab is still the previous one. Report the scope so the shell
      // can attribute a present to the pane the user is actually looking at.
      var presentation = controller.native && controller.native.presentation;
      var scope = presentation && typeof presentation.scope === "string"
        ? presentation.scope
        : null;
      // The dashboard only hands the stage to the embedded view while the
      // panel is in `interact` mode. In every other mode the dashboard paints
      // its own view, so the native child view has to stay hidden.
      var panelMode = controller.mode;
      if (typeof panelMode === "string" && panelMode !== "interact") { continue; }
      var style = window.getComputedStyle(stage);
      if (
        style.display === "none" ||
        style.visibility === "hidden" ||
        Number(style.opacity || "1") <= 0
      ) {
        continue;
      }
      var rect = stage.getBoundingClientRect();
      if (!(rect.width > 1 && rect.height > 1)) { continue; }
      var isActive = Boolean(pane && pane.classList.contains(ACTIVE_PANE_CLASS));
      if (best === null || (isActive && !best.active)) {
        best = { active: isActive, tabId: tabId, scope: scope, stage: stage, rect: rect };
        if (isActive) { break; }
      }
    }
    return best;
  }
  // The stage belongs to whichever pane is on screen, and it changes rect while
  // a window or splitter drag is in flight. Watch it directly so the native
  // child view follows the panel at paint speed instead of a poll interval.
  var probeStage = null;
  var probeStageObserver = null;
  function trackProbeStage(stage) {
    if (stage === probeStage) { return; }
    if (probeStageObserver) { probeStageObserver.disconnect(); }
    probeStageObserver = null;
    probeStage = stage;
    if (stage && typeof ResizeObserver === "function") {
      probeStageObserver = new ResizeObserver(function () { publishShellProbe(false); });
      probeStageObserver.observe(stage);
    }
  }
  // The shell only trusts a probe that was refreshed recently, so a stable
  // panel still has to prove it is alive: dedupe keeps the change-driven loop
  // cheap, and the heartbeat re-sends the identical payload on a timer.
  function publishShellProbe(force) {
    var probe = { visible: false, tabId: null, rect: null };
    var measurement = livePanelMeasurement();
    trackProbeStage(measurement ? measurement.stage : null);
    if (measurement) {
      probe = {
        visible: true,
        tabId: measurement.tabId,
        scope: measurement.scope,
        rect: {
          x: measurement.rect.x,
          y: measurement.rect.y,
          width: measurement.rect.width,
          height: measurement.rect.height,
        },
      };
    }
    var encoded = JSON.stringify(probe);
    if (!force && encoded === lastShellProbe) { return; }
    lastShellProbe = encoded;
    try {
      window.chrome.webview.postMessage(
        JSON.stringify({
          __starshipShellProbe: true,
          docId: shellDocumentId,
          probe: probe,
        }),
      );
    } catch (error) {
      /* The dashboard is navigating; the next tick retries. */
    }
  }
  var PROBE_HEARTBEAT_MS = 1000;
  var PROBE_POLL_MS = 250;
  window.setInterval(function () { publishShellProbe(false); }, PROBE_POLL_MS);
  window.setInterval(function () { publishShellProbe(true); }, PROBE_HEARTBEAT_MS);
  window.setTimeout(function () { publishShellProbe(true); }, 0);
  // Opening a tab and switching tabs are clicks; the dashboard updates its
  // active target only after the shell answers the open request, so re-measure
  // in a short burst instead of waiting for the next poll tick. The dedupe in
  // `publishShellProbe` keeps the extra ticks almost free.
  var probeBurst = null;
  function startProbeBurst() {
    if (probeBurst !== null) { window.clearInterval(probeBurst); }
    var ticks = 0;
    probeBurst = window.setInterval(function () {
      publishShellProbe(false);
      ticks += 1;
      if (ticks >= 8) {
        window.clearInterval(probeBurst);
        probeBurst = null;
      }
    }, 80);
  }
  document.addEventListener(
    "click",
    function () { startProbeBurst(); },
    true,
  );
  // Shell-owned driver API for Starship UI plugins and automated tasks.
  // The official dashboard never needs to call these; they are additive.
  window.openclawBrowserAct = function (action) {
    var payload = action && typeof action === "object" ? action : {};
    var message = {};
    for (var key in payload) {
      if (Object.prototype.hasOwnProperty.call(payload, key)) { message[key] = payload[key]; }
    }
    message.type = "act";
    return postMessage(message);
  };
  window.openclawBrowserDispatch = function (method, params, tabId) {
    return postMessage({
      type: "dispatch",
      method: method,
      params: params && typeof params === "object" ? params : {},
      tabId: tabId,
    });
  };
  window.openclawBrowserElements = function (tabId) {
    return postMessage({ type: "elements", tabId: tabId });
  };
})();
"#;

    /// Official `BrowserInspectScript.generated.swift` behavior, evaluated in
    /// the page instead of screenshot-space.
    const INSPECT_FUNCTION: &str = r#"
function openclawInspectBrowserElement(x, y) {
  const el = document.elementFromPoint(x, y);
  if (!el) { return null; }
  const rect = el.getBoundingClientRect();
  const label =
    el.getAttribute("aria-label") || el.getAttribute("alt") || el.getAttribute("title") || "";
  const text = (el.textContent || "").replace(/\s+/g, " ").trim();
  const nameSource = label || text;
  const nameLimit = 120;
  const nameEnd =
    (nameSource.codePointAt(nameLimit - 1) || 0) > 0xffff ? nameLimit - 1 : nameLimit;
  return {
    tag: el.tagName.toLowerCase(),
    id: el.id || "",
    classes: Array.from(el.classList).slice(0, 6),
    role: el.getAttribute("role") || "",
    name: nameSource.slice(0, nameEnd),
    rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
    focusable: typeof el.tabIndex === "number" && el.tabIndex >= 0,
  };
}
"#;

    #[derive(Default)]
    pub struct NativeBrowserState {
        bridge: Mutex<Option<Sender<Command>>>,
    }

    enum Command {
        Request { id: String, message: Value },
        TabEvent { tab_id: String, event: TabEvent },
        NewWindow { opener: String, url: String },
        ProcessFailed { tab_id: String, kind: i32 },
        /// Handshake from a dashboard document. `doc_id` identifies the
        /// document so the worker can replay the tab list exactly once per
        /// navigation instead of once per process.
        RestoreSession { doc_id: Option<String> },
        ShellProbe {
            visible: bool,
            tab_id: Option<String>,
            /// Presentation scope of the panel inside the pane on screen. The
            /// official dashboard keeps one scope per panel instance, so this
            /// is the only reliable way to attribute a `present` to the pane
            /// the user is looking at while that pane swaps tabs.
            scope: Option<String>,
            doc_id: Option<String>,
            rect: Option<Rect>,
        },
    }

    enum TabEvent {
        Url(String),
        Loading(bool),
        Title(String),
        History {
            can_go_back: bool,
            can_go_forward: bool,
        },
    }

    struct Tab {
        id: String,
        url: String,
        title: String,
        loading: bool,
        can_go_back: bool,
        can_go_forward: bool,
        opened_by: &'static str,
        opener_tab_id: Option<String>,
        webview: Webview,
    }

    #[derive(Clone, Copy, PartialEq)]
    struct Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    #[derive(Clone, PartialEq)]
    struct FallbackPresentation {
        tab_id: String,
        rect: Rect,
    }

    struct Presentation {
        tab_id: Option<String>,
        rect: Option<Rect>,
        visible: bool,
        order: u64,
    }

    /// The pane the user is actually looking at, as last reported by the shell
    /// probe that runs inside the dashboard document.
    #[derive(Clone)]
    struct ProbeSnapshot {
        visible: bool,
        tab_id: Option<String>,
        scope: Option<String>,
        at: Instant,
    }

    /// How long a probe stays usable as the authority for "which pane owns the
    /// native view". The injected script heartbeats once a second, so anything
    /// older than a few beats means the dashboard document is gone (navigation,
    /// unload) and the guard has to fall back to accepting every presentation.
    const PROBE_FRESHNESS: Duration = Duration::from_millis(3500);

    struct Worker {
        app: AppHandle,
        revision: u64,
        tabs: Vec<Tab>,
        scopes: HashMap<String, Presentation>,
        order: u64,
        seen: HashSet<String>,
        fallback: Option<FallbackPresentation>,
        /// Geometry last handed to each tab webview, so unchanged presentations
        /// do not spam the bridge log.
        applied: Vec<(String, Option<Rect>)>,
        /// Last shell probe, used to decide which dashboard pane owns the child
        /// WebView2. The dashboard keeps every visited chat pane mounted, so a
        /// cached pane can still announce itself; only the pane the user is
        /// looking at may move the native view.
        probe: Option<ProbeSnapshot>,
        active_tab_id: Option<String>,
        session_restored: bool,
        /// Dashboard document that last handshaked, so the tab list is replayed
        /// once per navigation instead of once per process.
        document_id: Option<String>,
    }

    /// Attaches the dashboard bridge handler and starts the worker once.
    pub fn install(app: AppHandle) {
        let Some(state) = app.try_state::<NativeBrowserState>() else {
            return;
        };
        let sender = {
            let mut bridge = match state.bridge.lock() {
                Ok(bridge) => bridge,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(sender) = bridge.as_ref() {
                sender.clone()
            } else {
                let (sender, receiver) = mpsc::channel();
                *bridge = Some(sender.clone());
                drop(bridge);
                let worker_app = app.clone();
                let result = thread::Builder::new()
                    .name("starship-native-browser".to_string())
                    .spawn(move || run_worker(worker_app, receiver));
                if result.is_err() {
                    return;
                }
                sender
            }
        };
        attach_dashboard_handler(&app, sender, 0);
    }

    /// How many times the WebView2 controller may not be ready before giving up.
    const ATTACH_RETRIES: u32 = 24;
    const ATTACH_RETRY_DELAY: Duration = Duration::from_millis(250);

    fn bridge_log(message: &str) {
        let Ok(local_app_data) = std::env::var("LOCALAPPDATA") else {
            return;
        };
        let path = std::path::PathBuf::from(local_app_data)
            .join("ai.starship.client")
            .join("native-browser.log");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{} {message}", timestamp());
        }
    }

    fn timestamp() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        format!("{}.{:03}", now.as_secs(), now.subsec_millis())
    }

    fn schedule_attach_retry(app: &AppHandle, sender: Sender<Command>, attempt: u32) {
        if attempt >= ATTACH_RETRIES {
            bridge_log(&format!("attach: giving up after {attempt} attempts"));
            return;
        }
        let app = app.clone();
        let _ = thread::Builder::new()
            .name("starship-native-browser-attach".to_string())
            .spawn(move || {
                thread::sleep(ATTACH_RETRY_DELAY);
                attach_dashboard_handler(&app, sender, attempt + 1);
            });
    }

    fn attach_dashboard_handler(app: &AppHandle, sender: Sender<Command>, attempt: u32) {
        let Some(webview) = app.get_webview("main") else {
            bridge_log(&format!("attach {attempt}: main webview missing"));
            schedule_attach_retry(app, sender, attempt);
            return;
        };
        let retry_app = app.clone();
        let retry_sender = sender.clone();
        let handler_sender = sender.clone();
        let result = webview.with_webview(move |platform| {
            let core = match unsafe { platform.controller().CoreWebView2() } {
                Ok(core) => core,
                Err(error) => {
                    bridge_log(&format!("attach {attempt}: CoreWebView2 unavailable: {error}"));
                    schedule_attach_retry(&retry_app, retry_sender, attempt);
                    return;
                }
            };
            let handler = WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
                let Some(args) = args else {
                    return Ok(());
                };
                let mut raw = PWSTR::null();
                if unsafe { args.WebMessageAsJson(&mut raw) }.is_err() {
                    return Ok(());
                }
                let text = take_pwstr(raw);
                let command = parse_inbound(&text);
                if !matches!(&command, Some(Command::ShellProbe { .. })) {
                    bridge_log(&format!(
                        "inbound: {}",
                        text.chars().take(240).collect::<String>()
                    ));
                }
                if let Some(command) = command {
                    let handshake = match &command {
                        Command::ShellProbe { doc_id, .. } => Some(doc_id.clone()),
                        _ => None,
                    };
                    let _ = handler_sender.send(command);
                    // The dashboard shim sends its first probe once it is
                    // injected, which is the earliest point where a restored
                    // tab session can be presented again.
                    if let Some(doc_id) = handshake {
                        let _ = handler_sender.send(Command::RestoreSession { doc_id });
                    }
                }
                Ok(())
            }));
            let mut token = 0i64;
            match unsafe { core.add_WebMessageReceived(&handler, &mut token) } {
                Ok(_) => bridge_log(&format!("attach {attempt}: web message handler ready token={token}")),
                Err(error) => {
                    bridge_log(&format!("attach {attempt}: add_WebMessageReceived failed: {error}"));
                    schedule_attach_retry(&retry_app, retry_sender, attempt);
                }
            }
        });
        if let Err(error) = result {
            bridge_log(&format!("attach {attempt}: with_webview failed: {error}"));
            schedule_attach_retry(app, sender, attempt);
        }
    }

    fn parse_inbound(text: &str) -> Option<Command> {
        let outer: Value = serde_json::from_str(text).ok()?;
        let value = match outer {
            Value::String(inner) => serde_json::from_str(&inner).ok()?,
            other => other,
        };
        if value.get("__starship") != Some(&Value::Bool(true)) {
            if value.get("__starshipShellProbe") != Some(&Value::Bool(true)) {
                return None;
            }
            let probe = value.get("probe")?;
            let visible = probe.get("visible")?.as_bool()?;
            let tab_id = match probe.get("tabId") {
                Some(Value::String(value))
                    if !value.is_empty() && value.trim() == value =>
                {
                    Some(value.clone())
                }
                _ => None,
            };
            let scope = match probe.get("scope") {
                Some(Value::String(value))
                    if !value.is_empty() && value.trim() == value =>
                {
                    Some(value.clone())
                }
                _ => None,
            };
            let rect = match probe.get("rect") {
                Some(Value::Null) | None => None,
                Some(value) => parse_rect(value),
            };
            let doc_id = match value.get("docId") {
                Some(Value::String(value))
                    if !value.is_empty() && value.trim() == value =>
                {
                    Some(value.clone())
                }
                _ => None,
            };
            return Some(Command::ShellProbe {
                visible,
                tab_id,
                scope,
                doc_id,
                rect,
            });
        }
        let id = value.get("id")?.as_str()?.to_string();
        let message = value.get("message")?.clone();
        Some(Command::Request { id, message })
    }

    fn run_worker(app: AppHandle, receiver: Receiver<Command>) {
        let mut worker = Worker {
            app,
            revision: 0,
            tabs: Vec::new(),
            scopes: HashMap::new(),
            order: 0,
            seen: HashSet::new(),
            fallback: None,
            applied: Vec::new(),
            probe: None,
            active_tab_id: None,
            session_restored: false,
            document_id: None,
        };
        while let Ok(command) = receiver.recv() {
            worker.handle(command);
        }
    }

    impl Worker {
        fn handle(&mut self, command: Command) {
            match command {
                Command::Request { id, message } => {
                    if !self.seen.insert(id.clone()) {
                        return;
                    }
                    let reply = self.handle_request(&message);
                    self.reply(&id, reply);
                }
                Command::TabEvent { tab_id, event } => {
                    let url_changed = matches!(event, TabEvent::Url(_));
                    if self.apply_event(&tab_id, event) {
                        self.push_state();
                        if url_changed {
                            self.persist_session();
                        }
                    }
                }
                Command::NewWindow { opener, url } => {
                    if valid_url(&url) {
                        let _ = self.open_tab(None, &url, "native", Some(opener));
                    }
                }
                Command::ProcessFailed { tab_id, kind } => {
                    self.recover_tab(&tab_id, kind);
                }
                Command::RestoreSession { doc_id } => {
                    if !self.session_restored {
                        self.session_restored = true;
                        self.restore_session();
                    }
                    // Replay the tab list once per dashboard document. The
                    // first restore lands while the shell still shows local
                    // bootstrap content, so the gateway document that replaces
                    // it would otherwise start with no tabs and render blank.
                    let fresh_document = doc_id.is_some() && doc_id != self.document_id;
                    self.document_id = doc_id.or_else(|| self.document_id.clone());
                    if fresh_document {
                        self.push_state();
                    }
                }
                Command::ShellProbe {
                    visible,
                    tab_id,
                    scope,
                    rect,
                    ..
                } => {
                    self.probe = Some(ProbeSnapshot {
                        visible,
                        tab_id: tab_id.clone(),
                        scope: scope.clone(),
                        at: Instant::now(),
                    });
                    let next = if visible {
                        match (tab_id, rect) {
                            (Some(tab_id), Some(rect)) => {
                                Some(FallbackPresentation { tab_id, rect })
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if self.fallback != next {
                        match &next {
                            Some(fallback) => bridge_log(&format!(
                                "shell fallback present tab={} rect=({:.0},{:.0},{:.0},{:.0})",
                                fallback.tab_id,
                                fallback.rect.x,
                                fallback.rect.y,
                                fallback.rect.width,
                                fallback.rect.height
                            )),
                            None => bridge_log("shell fallback cleared"),
                        }
                        self.fallback = next;
                        self.apply_presentations();
                    }
                    if let Some(active) = self
                        .fallback
                        .as_ref()
                        .map(|fallback| fallback.tab_id.clone())
                    {
                        self.note_active_tab(&active);
                    }
                }
            }
        }

        fn handle_request(&mut self, message: &Value) -> Value {
            let Some(kind) = message.get("type").and_then(Value::as_str) else {
                return invalid_request();
            };
            match kind {
                "open" => {
                    let Some(url) = message.get("url").and_then(Value::as_str) else {
                        return invalid_request();
                    };
                    if !valid_url(url) {
                        return invalid_request();
                    }
                    let requested = message
                        .get("tabId")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    match self.open_tab(requested, url, "web", None) {
                        Ok(tab_id) => json!({ "ok": true, "tabId": tab_id }),
                        Err(error) => json!({ "ok": false, "error": error }),
                    }
                }
                "navigate" => {
                    let Some(tab_id) = self.tab_id(message) else {
                        return invalid_request();
                    };
                    let Some(raw_url) = message.get("url").and_then(Value::as_str) else {
                        return invalid_request();
                    };
                    let Some(url) = sanitize_url(raw_url) else {
                        return invalid_request();
                    };
                    let Some(webview) = self.webview(&tab_id) else {
                        return unknown_tab();
                    };
                    match Url::parse(&url) {
                        Ok(url) => match webview.navigate(url) {
                            Ok(()) => json!({ "ok": true }),
                            Err(error) => {
                                json!({ "ok": false, "error": format!("Could not navigate: {error}") })
                            }
                        },
                        Err(_) => invalid_request(),
                    }
                }
                "back" | "forward" | "reload" | "stop" => {
                    let Some(tab_id) = self.tab_id(message) else {
                        return invalid_request();
                    };
                    let Some(webview) = self.webview(&tab_id) else {
                        return unknown_tab();
                    };
                    let result: Option<Result<(), String>> = match kind {
                        "back" => run_on_core(&webview, |core| unsafe { core.GoBack() })
                            .map(|result| result.map_err(|error| error.to_string())),
                        "forward" => run_on_core(&webview, |core| unsafe { core.GoForward() })
                            .map(|result| result.map_err(|error| error.to_string())),
                        "reload" => Some(webview.reload().map_err(|error| error.to_string())),
                        _ => run_on_core(&webview, |core| unsafe { core.Stop() })
                            .map(|result| result.map_err(|error| error.to_string())),
                    };
                    match result {
                        Some(Ok(())) => json!({ "ok": true }),
                        Some(Err(error)) => {
                            json!({ "ok": false, "error": format!("Native browser command failed: {error}") })
                        }
                        None => unknown_tab(),
                    }
                }
                "close" => {
                    let Some(tab_id) = self.tab_id(message) else {
                        return invalid_request();
                    };
                    match self.close_tab(&tab_id) {
                        Ok(()) => json!({ "ok": true }),
                        Err(error) => json!({ "ok": false, "error": error }),
                    }
                }
                "snapshot" => {
                    let Some(tab_id) = self.tab_id(message) else {
                        return invalid_request();
                    };
                    let Some(webview) = self.webview(&tab_id) else {
                        return unknown_tab();
                    };
                    match snapshot(&webview) {
                        Ok(reply) => reply,
                        Err(error) => json!({ "ok": false, "error": error }),
                    }
                }
                "elements" => {
                    let Some(tab_id) = self.tab_id(message) else {
                        return invalid_request();
                    };
                    let Some(webview) = self.webview(&tab_id) else {
                        return unknown_tab();
                    };
                    match elements(&webview) {
                        Ok(reply) => reply,
                        Err(error) => json!({ "ok": false, "error": error }),
                    }
                }
                "dispatch" => {
                    let Some(tab_id) = self.tab_id(message) else {
                        return invalid_request();
                    };
                    let Some(method) = message.get("method").and_then(Value::as_str) else {
                        return invalid_request();
                    };
                    if !allowed_cdp_method(method) {
                        return invalid_request();
                    }
                    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
                    let encoded = cdp_params(params);
                    if encoded.len() > 100_000 {
                        return invalid_request();
                    }
                    let Some(webview) = self.webview(&tab_id) else {
                        return unknown_tab();
                    };
                    match call_cdp(&webview, method, &encoded) {
                        Some(raw) => {
                            let result =
                                serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw));
                            json!({ "ok": true, "method": method, "result": result })
                        }
                        None => json!({ "ok": false, "error": format!("CDP {method} failed") }),
                    }
                }
                "act" => {
                    let Some(tab_id) = self.tab_id(message) else {
                        return invalid_request();
                    };
                    let Some(action) = message.get("action").and_then(Value::as_str) else {
                        return invalid_request();
                    };
                    let Some(webview) = self.webview(&tab_id) else {
                        return unknown_tab();
                    };
                    match perform_act(&webview, action, message) {
                        Ok(detail) => json!({
                            "ok": true,
                            "effect": "confirmed",
                            "action": action,
                            "detail": detail,
                            "page": page_state(&webview),
                        }),
                        Err(error) => json!({ "ok": false, "error": error }),
                    }
                }
                "inspect" => {
                    let Some(tab_id) = self.tab_id(message) else {
                        return invalid_request();
                    };
                    let Some(x) = message.get("x").and_then(Value::as_f64) else {
                        return invalid_request();
                    };
                    let Some(y) = message.get("y").and_then(Value::as_f64) else {
                        return invalid_request();
                    };
                    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
                        return invalid_request();
                    }
                    let Some(webview) = self.webview(&tab_id) else {
                        return unknown_tab();
                    };
                    let script = format!("(() => {{ {INSPECT_FUNCTION}\nreturn openclawInspectBrowserElement({x}, {y}); }})()");
                    match execute_script(&webview, script) {
                        Some(raw) => match serde_json::from_str::<Value>(&raw) {
                            Ok(node) => json!({ "ok": true, "node": node }),
                            Err(_) => json!({ "ok": false, "error": "Native browser inspection failed" }),
                        },
                        None => json!({ "ok": false, "error": "Native browser inspection failed" }),
                    }
                }
                "present" => {
                    let Some(scope) = nonempty(message.get("scope")) else {
                        return invalid_request();
                    };
                    let tab_id = match message.get("tabId") {
                        Some(Value::Null) => None,
                        Some(Value::String(value)) if !value.is_empty() && value.trim() == value => {
                            Some(value.clone())
                        }
                        _ => return invalid_request(),
                    };
                    let rect = match message.get("rect") {
                        Some(Value::Null) => None,
                        Some(value) => match parse_rect(value) {
                            Some(rect) => Some(rect),
                            None => return invalid_request(),
                        },
                        None => return invalid_request(),
                    };
                    let Some(visible) = message.get("visible").and_then(Value::as_bool) else {
                        return invalid_request();
                    };
                    // Every visited chat pane stays mounted with its own
                    // browser panel, and a cached pane can still announce
                    // itself with valid geometry. The present is always
                    // recorded — the dashboard de-duplicates payloads, so a
                    // dropped present is never retried and the panel would be
                    // left without geometry — but only the scope reported by
                    // the pane on screen may drive the native view; see
                    // `presentation_winners`.
                    if let Some(live) = self.live_scope() {
                        if live != scope {
                            bridge_log(&format!(
                                "shell present deferred scope={scope} live={live} tab={}",
                                tab_id.as_deref().unwrap_or("-")
                            ));
                        }
                    }
                    self.order += 1;
                    self.scopes.insert(
                        scope,
                        Presentation {
                            tab_id,
                            rect,
                            visible,
                            order: self.order,
                        },
                    );
                    self.apply_presentations();
                    if visible {
                        if let Some(active) = self.shown_tab() {
                            self.note_active_tab(&active);
                        }
                    }
                    json!({ "ok": true })
                }
                "release-scope" => {
                    let Some(scope) = nonempty(message.get("scope")) else {
                        return invalid_request();
                    };
                    self.scopes.remove(&scope);
                    self.apply_presentations();
                    json!({ "ok": true })
                }
                _ => invalid_request(),
            }
        }

        fn tab_id(&self, message: &Value) -> Option<String> {
            nonempty(message.get("tabId"))
        }

        /// The probe of the pane the user is looking at, while it is still
        /// fresh. A stale or missing probe yields `None` so the shell falls
        /// back to plain attribution instead of freezing the last geometry.
        fn live_probe(&self) -> Option<&ProbeSnapshot> {
            let probe = self.probe.as_ref()?;
            if !probe.visible || probe.at.elapsed() > PROBE_FRESHNESS {
                return None;
            }
            Some(probe)
        }

        /// Presentation scope of the pane on screen, for logging and for
        /// attributing a `present` to the pane that actually issued it.
        fn live_scope(&self) -> Option<&str> {
            self.live_probe()?.scope.as_deref()
        }

        /// Tab the native view is currently showing, derived from the same
        /// winners `apply_presentations` hands to the tab webviews.
        fn shown_tab(&self) -> Option<String> {
            self.presentation_winners()
                .into_iter()
                .max_by_key(|(_, (order, _))| *order)
                .map(|(tab_id, _)| tab_id)
        }

        /// Geometry the shell should hand to each tab webview right now.
        ///
        /// The pane the user is looking at is the only pane allowed to drive
        /// the child view, and its probe is the freshest truth about which tab
        /// it shows. That matters because the dashboard de-duplicates
        /// presentations (`lastPayload`) and only re-sends when the payload
        /// changes: a record left behind by the pane's previous tab, or a
        /// record from a cached pane, would otherwise keep the wrong view on
        /// screen forever. While no probe is usable the shell falls back to
        /// order-based attribution so the panel still opens during startup.
        fn presentation_winners(&self) -> HashMap<String, (u64, Rect)> {
            let mut winners: HashMap<String, (u64, Rect)> = HashMap::new();
            if let Some(probe) = self.live_probe() {
                // Every probe that reports a visible panel mirrors its tab and
                // rect into `fallback` on arrival, so the fallback carries the
                // geometry of the pane on screen.
                if let (Some(tab_id), Some(fallback)) =
                    (probe.tab_id.as_deref(), self.fallback.as_ref())
                {
                    winners.insert(tab_id.to_string(), (u64::MAX, fallback.rect));
                }
                return winners;
            }
            for scope in self.scopes.values() {
                if !scope.visible {
                    continue;
                }
                let (Some(tab_id), Some(rect)) = (scope.tab_id.as_deref(), scope.rect) else {
                    continue;
                };
                match winners.get(tab_id) {
                    Some((order, _)) if *order >= scope.order => {}
                    _ => {
                        winners.insert(tab_id.to_string(), (scope.order, rect));
                    }
                }
            }
            // Without a live probe the dashboard stays authoritative whenever
            // it presents a tab; the fallback only bridges the input-ownership
            // gap where the panel is visibly open but nothing is presented.
            if winners.is_empty() {
                if let Some(fallback) = &self.fallback {
                    winners.insert(fallback.tab_id.clone(), (u64::MAX, fallback.rect));
                }
            }
            winners
        }

        fn webview(&self, tab_id: &str) -> Option<Webview> {
            self.tabs
                .iter()
                .find(|tab| tab.id == tab_id)
                .map(|tab| tab.webview.clone())
        }

        fn open_tab(
            &mut self,
            requested: Option<String>,
            url: &str,
            opened_by: &'static str,
            opener: Option<String>,
        ) -> Result<String, String> {
            // Sanitizing here (not only at the request boundary) also repairs
            // tabs that were persisted before this guard existed.
            let url = sanitize_url(url).ok_or_else(|| "Invalid native browser URL".to_string())?;
            if url != "about:blank" {
                if let Some(tab) = self.tabs.iter().find(|tab| tab.url == url) {
                    return Ok(tab.id.clone());
                }
            }
            let id = self.unique_id(requested);
            let label = format!("native-browser-{}", sanitize_label(&id));
            let target = Url::parse(&url).map_err(|_| "Invalid native browser URL".to_string())?;
            let window = self
                .app
                .get_window("main")
                .ok_or_else(|| "Main window is unavailable.".to_string())?;
            let popup_sender = self.sender()?;
            let popup_tab = id.clone();
            let builder = WebviewBuilder::new(label, WebviewUrl::External(target))
                .focused(false)
                .on_new_window(move |url, _features| {
                    let _ = popup_sender.send(Command::NewWindow {
                        opener: popup_tab.clone(),
                        url: url.to_string(),
                    });
                    NewWindowResponse::Deny
                });
            let webview = window
                .add_child(
                    builder,
                    LogicalPosition::new(-32000.0, -32000.0),
                    LogicalSize::new(1.0, 1.0),
                )
                .map_err(|error| format!("Could not create native browser tab: {error}"))?;
            let sender = self.sender()?;
            attach_tab_events(&webview, &id, sender);
            self.tabs.push(Tab {
                id: id.clone(),
                url,
                title: String::new(),
                loading: true,
                can_go_back: false,
                can_go_forward: false,
                opened_by,
                opener_tab_id: opener,
                webview,
            });
            self.push_state();
            Ok(id)
        }

        fn close_tab(&mut self, tab_id: &str) -> Result<(), String> {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
                return Err("Unknown native browser tab".to_string());
            };
            let tab = self.tabs.remove(index);
            let _ = tab.webview.close();
            self.scopes
                .retain(|_, scope| scope.tab_id.as_deref() != Some(tab_id));
            if self.active_tab_id.as_deref() == Some(tab_id) {
                self.active_tab_id = None;
            }
            self.apply_presentations();
            self.push_state();
            self.persist_session();
            Ok(())
        }

        /// Keeps a crashed panel tab usable instead of leaving a blank frame.
        ///
        /// A crashed renderer usually recovers with a reload, but a browser
        /// process exit destroys the controller itself, so that tab has to be
        /// rebuilt from its last known URL.
        fn recover_tab(&mut self, tab_id: &str, kind: i32) {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
                return;
            };
            let browser_process_exited = kind == 0;
            let url = {
                let tab = &self.tabs[index];
                if tab.url.is_empty() {
                    "about:blank".to_string()
                } else {
                    tab.url.clone()
                }
            };
            let opened_by = self.tabs[index].opened_by;
            let opener = self.tabs[index].opener_tab_id.clone();
            if !browser_process_exited {
                match self.tabs[index].webview.reload() {
                    Ok(()) => {
                        bridge_log(&format!(
                            "process failed kind={kind} tab={tab_id} recovered by reload"
                        ));
                        self.tabs[index].loading = true;
                        self.push_state();
                        return;
                    }
                    Err(error) => bridge_log(&format!(
                        "process failed kind={kind} tab={tab_id} reload failed: {error}"
                    )),
                }
            }
            bridge_log(&format!(
                "process failed kind={kind} tab={tab_id} rebuilding webview url={url}"
            ));
            let _ = self.close_tab(tab_id);
            match self.open_tab(Some(tab_id.to_string()), &url, opened_by, opener) {
                Ok(rebuilt) => bridge_log(&format!(
                    "process failed tab={tab_id} rebuilt as {rebuilt}"
                )),
                Err(error) => bridge_log(&format!(
                    "process failed tab={tab_id} rebuild failed: {error}"
                )),
            }
        }

        fn apply_event(&mut self, tab_id: &str, event: TabEvent) -> bool {
            let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                return false;
            };
            match event {
                TabEvent::Url(url) => {
                    if let Some(url) = sanitize_url(&url) {
                        tab.url = url;
                    }
                }
                TabEvent::Loading(loading) => tab.loading = loading,
                TabEvent::Title(title) => tab.title = title,
                TabEvent::History {
                    can_go_back,
                    can_go_forward,
                } => {
                    tab.can_go_back = can_go_back;
                    tab.can_go_forward = can_go_forward;
                }
            }
            true
        }

        fn apply_presentations(&mut self) {
            let winners = self.presentation_winners();
            let source = if self.live_probe().is_some() {
                "probe"
            } else {
                "dashboard"
            };
            let mut applied: Vec<(String, Option<Rect>)> = Vec::new();
            for tab in &self.tabs {
                match winners.get(&tab.id) {
                    Some((_, rect)) => {
                        let _ = tab
                            .webview
                            .set_position(LogicalPosition::new(rect.x, rect.y));
                        let _ = tab.webview.set_size(LogicalSize::new(
                            rect.width.max(1.0),
                            rect.height.max(1.0),
                        ));
                        let _ = tab.webview.show();
                        applied.push((tab.id.clone(), Some(*rect)));
                    }
                    None => {
                        let _ = tab.webview.hide();
                        applied.push((tab.id.clone(), None));
                    }
                }
            }
            // Log the geometry the native view actually received. The child
            // WebView2 has no readable bounds through CDP, so this line is the
            // only end-to-end proof that the panel on screen and the native
            // view agree; it also makes alignment regressions bisectable.
            if applied != self.applied {
                for (tab_id, rect) in &applied {
                    match rect {
                        Some(rect) => bridge_log(&format!(
                            "shell apply tab={tab_id} rect=({:.0},{:.0},{:.0},{:.0}) source={source}",
                            rect.x, rect.y, rect.width, rect.height,
                        )),
                        None => bridge_log(&format!("shell hide tab={tab_id} source={source}")),
                    }
                }
                self.applied = applied;
            }
        }

        fn sender(&self) -> Result<Sender<Command>, String> {
            let Some(state) = self.app.try_state::<NativeBrowserState>() else {
                return Err("Native browser bridge is unavailable".to_string());
            };
            let bridge = match state.bridge.lock() {
                Ok(bridge) => bridge,
                Err(poisoned) => poisoned.into_inner(),
            };
            bridge
                .as_ref()
                .cloned()
                .ok_or_else(|| "Native browser bridge is unavailable".to_string())
        }

        fn unique_id(&self, requested: Option<String>) -> String {
            if let Some(requested) = requested {
                if !requested.is_empty()
                    && requested.trim() == requested
                    && !self.tabs.iter().any(|tab| tab.id == requested)
                {
                    return requested;
                }
            }
            format!("starship-{}", uuid::Uuid::new_v4())
        }

        fn session_path(&self) -> Result<std::path::PathBuf, String> {
            let base = self
                .app
                .path()
                .app_local_data_dir()
                .or_else(|_| {
                    std::env::var("LOCALAPPDATA").map(std::path::PathBuf::from).map_err(|_| {
                        tauri::Error::AssetNotFound(
                            "native browser session directory is unavailable".to_string(),
                        )
                    })
                })
                .map_err(|error| error.to_string())?;
            Ok(base.join("browser-session.json"))
        }

        fn persist_session(&self) {
            let Ok(path) = self.session_path() else {
                return;
            };
            let tabs: Vec<Value> = self
                .tabs
                .iter()
                .filter(|tab| tab.url != "about:blank")
                .map(|tab| {
                    json!({
                        "id": tab.id,
                        "url": tab.url,
                        "openedBy": tab.opened_by,
                        "openerTabId": tab.opener_tab_id,
                    })
                })
                .collect();
            let active = self
                .active_tab_id
                .as_deref()
                .filter(|id| self.tabs.iter().any(|tab| tab.id == *id));
            let session = json!({ "version": 1, "activeTabId": active, "tabs": tabs });
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_vec(&session) {
                Ok(encoded) => {
                    if let Err(error) = std::fs::write(&path, encoded) {
                        bridge_log(&format!("session persist failed: {error}"));
                    }
                }
                Err(error) => bridge_log(&format!("session encode failed: {error}")),
            }
        }

        fn note_active_tab(&mut self, tab_id: &str) {
            if self.active_tab_id.as_deref() == Some(tab_id) {
                return;
            }
            self.active_tab_id = Some(tab_id.to_string());
            self.persist_session();
        }

        /// Restores the tabs that were open before the client was closed.
        fn restore_session(&mut self) {
            if !self.tabs.is_empty() {
                return;
            }
            let Ok(path) = self.session_path() else {
                return;
            };
            let Ok(raw) = std::fs::read_to_string(&path) else {
                return;
            };
            let Ok(session) = serde_json::from_str::<Value>(&raw) else {
                bridge_log("session restore skipped: unreadable state");
                return;
            };
            let Some(entries) = session.get("tabs").and_then(Value::as_array) else {
                return;
            };
            let active = session
                .get("activeTabId")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut restored = 0usize;
            for entry in entries {
                let Some(url) = entry.get("url").and_then(Value::as_str) else {
                    continue;
                };
                if !valid_url(url) || url == "about:blank" {
                    continue;
                }
                let requested = entry.get("id").and_then(Value::as_str).map(str::to_string);
                let opened_by = match entry.get("openedBy").and_then(Value::as_str) {
                    Some("native") => "native",
                    _ => "web",
                };
                let opener = entry
                    .get("openerTabId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if self.open_tab(requested, url, opened_by, opener).is_ok() {
                    restored += 1;
                }
            }
            if let Some(active) = active {
                self.active_tab_id = Some(active);
            }
            if restored > 0 {
                bridge_log(&format!("session restored tabs={restored}"));
            }
        }

        fn reply(&self, id: &str, reply: Value) {
            post_to_dashboard(
                &self.app,
                &json!({ "__starshipReply": true, "id": id, "reply": reply }),
            );
        }

        fn push_state(&mut self) {
            self.revision += 1;
            let tabs: Vec<Value> = self
                .tabs
                .iter()
                .map(|tab| {
                    let mut value = json!({
                        "id": tab.id,
                        "url": tab.url,
                        "title": tab.title,
                        "loading": tab.loading,
                        "canGoBack": tab.can_go_back,
                        "canGoForward": tab.can_go_forward,
                        "openedBy": tab.opened_by,
                    });
                    if let Some(opener) = &tab.opener_tab_id {
                        value["openerTabId"] = json!(opener);
                    }
                    value
                })
                .collect();
            post_to_dashboard(
                &self.app,
                &json!({
                    "__starshipState": true,
                    "state": { "revision": self.revision, "tabs": tabs },
                }),
            );
        }
    }

    fn attach_tab_events(webview: &Webview, tab_id: &str, sender: Sender<Command>) {
        let webview = webview.clone();
        let tab_id = tab_id.to_string();
        let _ = webview.with_webview(move |platform| {
            let Ok(core) = (unsafe { platform.controller().CoreWebView2() }) else {
                return;
            };
            unsafe {
                let mut token = 0i64;

                let nav_sender = sender.clone();
                let nav_tab = tab_id.clone();
                let handler =
                    NavigationStartingEventHandler::create(Box::new(move |_sender, args| {
                        if let Some(args) = args {
                            let mut uri = PWSTR::null();
                            if args.Uri(&mut uri).is_ok() {
                                let _ = nav_sender.send(Command::TabEvent {
                                    tab_id: nav_tab.clone(),
                                    event: TabEvent::Url(take_pwstr(uri)),
                                });
                            }
                        }
                        let _ = nav_sender.send(Command::TabEvent {
                            tab_id: nav_tab.clone(),
                            event: TabEvent::Loading(true),
                        });
                        Ok(())
                    }));
                let _ = core.add_NavigationStarting(&handler, &mut token);

                let content_sender = sender.clone();
                let content_tab = tab_id.clone();
                let handler = ContentLoadingEventHandler::create(Box::new(move |_sender, _args| {
                    let _ = content_sender.send(Command::TabEvent {
                        tab_id: content_tab.clone(),
                        event: TabEvent::Loading(true),
                    });
                    Ok(())
                }));
                let _ = core.add_ContentLoading(&handler, &mut token);

                let source_sender = sender.clone();
                let source_tab = tab_id.clone();
                let handler = SourceChangedEventHandler::create(Box::new(move |sender, _args| {
                    if let Some(sender) = sender {
                        let mut uri = PWSTR::null();
                        if sender.Source(&mut uri).is_ok() {
                            let _ = source_sender.send(Command::TabEvent {
                                tab_id: source_tab.clone(),
                                event: TabEvent::Url(take_pwstr(uri)),
                            });
                        }
                    }
                    Ok(())
                }));
                let _ = core.add_SourceChanged(&handler, &mut token);

                let completed_sender = sender.clone();
                let completed_tab = tab_id.clone();
                let handler =
                    NavigationCompletedEventHandler::create(Box::new(move |sender, _args| {
                        let _ = completed_sender.send(Command::TabEvent {
                            tab_id: completed_tab.clone(),
                            event: TabEvent::Loading(false),
                        });
                        if let Some(sender) = sender {
                            let _ = completed_sender.send(Command::TabEvent {
                                tab_id: completed_tab.clone(),
                                event: history_event(&sender),
                            });
                        }
                        Ok(())
                    }));
                let _ = core.add_NavigationCompleted(&handler, &mut token);

                let title_sender = sender.clone();
                let title_tab = tab_id.clone();
                let handler =
                    DocumentTitleChangedEventHandler::create(Box::new(move |sender, _args| {
                        if let Some(sender) = sender {
                            let mut title = PWSTR::null();
                            if sender.DocumentTitle(&mut title).is_ok() {
                                let _ = title_sender.send(Command::TabEvent {
                                    tab_id: title_tab.clone(),
                                    event: TabEvent::Title(take_pwstr(title)),
                                });
                            }
                        }
                        Ok(())
                    }));
                let _ = core.add_DocumentTitleChanged(&handler, &mut token);

                let history_sender = sender.clone();
                let history_tab = tab_id.clone();
                let handler = HistoryChangedEventHandler::create(Box::new(move |sender, _args| {
                    if let Some(sender) = sender {
                        let _ = history_sender.send(Command::TabEvent {
                            tab_id: history_tab.clone(),
                            event: history_event(&sender),
                        });
                    }
                    Ok(())
                }));
                let _ = core.add_HistoryChanged(&handler, &mut token);

                // A crashed WebView2 process leaves an empty rectangle where
                // the panel should be. Recover in place instead of waiting for
                // the user to close and reopen the panel.
                let failed_sender = sender.clone();
                let failed_tab = tab_id.clone();
                let handler =
                    ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
                        let kind = args
                            .and_then(|args| {
                                let mut kind = Default::default();
                                args.ProcessFailedKind(&mut kind).ok()?;
                                Some(kind.0)
                            })
                            .unwrap_or(-1);
                        let _ = failed_sender.send(Command::ProcessFailed {
                            tab_id: failed_tab.clone(),
                            kind,
                        });
                        Ok(())
                    }));
                let _ = core.add_ProcessFailed(&handler, &mut token);

                let popup_sender = sender;
                let popup_tab = tab_id;
                let handler =
                    NewWindowRequestedEventHandler::create(Box::new(move |_sender, args| {
                        if let Some(args) = args {
                            let mut uri = PWSTR::null();
                            if args.Uri(&mut uri).is_ok() {
                                let _ = popup_sender.send(Command::NewWindow {
                                    opener: popup_tab.clone(),
                                    url: take_pwstr(uri),
                                });
                            }
                            let _ = args.SetHandled(true);
                        }
                        Ok(())
                    }));
                let _ = core.add_NewWindowRequested(&handler, &mut token);
            }
        });
    }

    fn history_event(core: &ICoreWebView2) -> TabEvent {
        let mut back = BOOL::default();
        let mut forward = BOOL::default();
        let _ = unsafe { core.CanGoBack(&mut back) };
        let _ = unsafe { core.CanGoForward(&mut forward) };
        TabEvent::History {
            can_go_back: back.as_bool(),
            can_go_forward: forward.as_bool(),
        }
    }

    fn snapshot(webview: &Webview) -> Result<Value, String> {
        let raw = execute_script(
            webview,
            "({ width: window.innerWidth, height: window.innerHeight })".to_string(),
        )
        .ok_or_else(|| "Native browser snapshot failed".to_string())?;
        let metrics: Value = serde_json::from_str(&raw)
            .map_err(|_| "Native browser snapshot failed".to_string())?;
        let width = metrics.get("width").and_then(Value::as_f64).unwrap_or(0.0);
        let height = metrics.get("height").and_then(Value::as_f64).unwrap_or(0.0);
        if width <= 0.0 || height <= 0.0 {
            return Err("Native browser tab is not visible".to_string());
        }
        let data = capture_png(webview)
            .ok_or_else(|| "Native browser snapshot failed".to_string())?;
        Ok(json!({
            "ok": true,
            "dataUrl": format!("data:image/png;base64,{data}"),
            "cssWidth": width,
            "cssHeight": height,
        }))
    }

    fn elements(webview: &Webview) -> Result<Value, String> {
        let script = r#"(() => {
  if (!window.__starshipRefSeq) { window.__starshipRefSeq = 0; }
  const selector =
    "a,button,input,textarea,select,[role=button],[role=link],[role=textbox],[contenteditable=true],[tabindex]";
  const nodes = Array.from(document.querySelectorAll(selector));
  const items = [];
  for (const el of nodes) {
    const rect = el.getBoundingClientRect();
    if (rect.width < 2 || rect.height < 2) { continue; }
    const style = window.getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden" || Number(style.opacity || "1") === 0) {
      continue;
    }
    let ref = el.getAttribute("data-starship-ref");
    if (!ref) {
      ref = "sr-" + (++window.__starshipRefSeq);
      el.setAttribute("data-starship-ref", ref);
    }
    items.push({
      ref: ref,
      tag: el.tagName.toLowerCase(),
      role: el.getAttribute("role") || "",
      name: (el.getAttribute("aria-label") || el.getAttribute("title") || el.textContent || "")
        .replace(/\s+/g, " ").trim().slice(0, 120),
      value: typeof el.value === "string" ? el.value.slice(0, 120) : null,
      disabled: el.disabled === true,
      rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
    });
    if (items.length >= 200) { break; }
  }
  return { count: items.length, elements: items };
})()"#;
        let raw = execute_script(webview, script.to_string())
            .ok_or_else(|| "Native browser element scan failed".to_string())?;
        serde_json::from_str::<Value>(&raw)
            .map_err(|_| "Native browser element scan failed".to_string())
    }

    fn page_state(webview: &Webview) -> Value {
        let script = r#"(() => {
  const el = document.activeElement;
  return {
    url: location.href,
    title: document.title,
    scrollX: window.scrollX,
    scrollY: window.scrollY,
    activeElement: el
      ? {
          tag: el.tagName.toLowerCase(),
          ref: el.getAttribute("data-starship-ref"),
          value: typeof el.value === "string" ? el.value.slice(0, 200) : null,
        }
      : null,
  };
})()"#;
        execute_script(webview, script.to_string())
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or(Value::Null)
    }

    fn call_cdp(webview: &Webview, method: &str, params: &str) -> Option<String> {
        let (sender, receiver) = mpsc::channel();
        let method = HSTRING::from(method.to_string());
        let parameters = HSTRING::from(params.to_string());
        webview
            .with_webview(move |platform| {
                let core = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => core,
                    Err(_) => {
                        let _ = sender.send(None);
                        return;
                    }
                };
                let handler_sender = sender.clone();
                let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
                    move |_error, result| {
                        let _ = handler_sender.send(Some(result));
                        Ok(())
                    },
                ));
                if unsafe { core.CallDevToolsProtocolMethod(&method, &parameters, &handler) }
                    .is_err()
                {
                    let _ = sender.send(None);
                }
            })
            .ok()?;
        receiver.recv_timeout(Duration::from_secs(10)).ok().flatten()
    }

    fn allowed_cdp_method(method: &str) -> bool {
        const PREFIXES: [&str; 5] = ["Input.", "Page.", "DOM.", "Runtime.", "Network."];
        method.len() < 64
            && method
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
            && PREFIXES.iter().any(|prefix| method.starts_with(prefix))
    }

    fn valid_element_ref(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    }

    fn cdp_params(value: Value) -> String {
        serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
    }

    fn finite_point(message: &Value) -> Option<(f64, f64)> {
        let x = message.get("x").and_then(Value::as_f64)?;
        let y = message.get("y").and_then(Value::as_f64)?;
        if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
            return None;
        }
        Some((x, y))
    }

    fn element_point(webview: &Webview, reference: &str) -> Result<(f64, f64), String> {
        let script = format!(
            "(() => {{ const el = document.querySelector('[data-starship-ref=\"{reference}\"]'); \
             if (!el) return null; \
             el.scrollIntoView({{ block: \"center\", inline: \"center\" }}); \
             const rect = el.getBoundingClientRect(); \
             if (rect.width <= 0 || rect.height <= 0) return null; \
             return {{ x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 }}; }})()"
        );
        let raw =
            execute_script(webview, script).ok_or_else(|| "Element lookup failed".to_string())?;
        let value: Value =
            serde_json::from_str(&raw).map_err(|_| "Element lookup failed".to_string())?;
        let x = value
            .get("x")
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("Element reference {reference} was not found"))?;
        let y = value
            .get("y")
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("Element reference {reference} was not found"))?;
        Ok((x, y))
    }

    fn act_point(webview: &Webview, message: &Value) -> Result<(f64, f64), String> {
        if let Some(reference) = message.get("elementRef").and_then(Value::as_str) {
            if !valid_element_ref(reference) {
                return Err("Invalid element reference".to_string());
            }
            return element_point(webview, reference);
        }
        finite_point(message)
            .ok_or_else(|| "A valid elementRef or x/y point is required".to_string())
    }

    fn perform_click(
        webview: &Webview,
        x: f64,
        y: f64,
        button: &str,
        click_count: u64,
    ) -> Result<(), String> {
        let events = [
            ("mouseMoved", 0_u64),
            ("mousePressed", click_count),
            ("mouseReleased", click_count),
        ];
        for (event_type, count) in events {
            let buttons = if event_type == "mousePressed" { 1 } else { 0 };
            let params = json!({
                "type": event_type,
                "x": x,
                "y": y,
                "button": button,
                "buttons": buttons,
                "clickCount": count,
            });
            call_cdp(webview, "Input.dispatchMouseEvent", &cdp_params(params))
                .ok_or_else(|| format!("CDP {event_type} failed"))?;
        }
        Ok(())
    }

    fn key_definition(key: &str) -> Option<(&'static str, i64, &'static str)> {
        let definition = match key {
            "Enter" => ("Enter", 13, "\r"),
            "Tab" => ("Tab", 9, "\t"),
            "Escape" => ("Escape", 27, ""),
            "Backspace" => ("Backspace", 8, ""),
            "Delete" => ("Delete", 46, ""),
            "ArrowUp" => ("ArrowUp", 38, ""),
            "ArrowDown" => ("ArrowDown", 40, ""),
            "ArrowLeft" => ("ArrowLeft", 37, ""),
            "ArrowRight" => ("ArrowRight", 39, ""),
            "Home" => ("Home", 36, ""),
            "End" => ("End", 35, ""),
            "PageUp" => ("PageUp", 33, ""),
            "PageDown" => ("PageDown", 34, ""),
            "Space" => ("Space", 32, " "),
            _ => return None,
        };
        Some(definition)
    }

    fn wait_for_selector(webview: &Webview, selector: &str, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let quoted = serde_json::to_string(selector).unwrap_or_else(|_| "null".to_string());
        loop {
            let script = format!("Boolean(document.querySelector({quoted}))");
            if execute_script(webview, script).as_deref() == Some("true") {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(120));
        }
    }

    fn perform_act(webview: &Webview, action: &str, message: &Value) -> Result<Value, String> {
        match action {
            "click" => {
                let (x, y) = act_point(webview, message)?;
                let button = message
                    .get("button")
                    .and_then(Value::as_str)
                    .unwrap_or("left");
                if !matches!(button, "left" | "right" | "middle") {
                    return Err("Invalid mouse button".to_string());
                }
                let click_count = message
                    .get("clickCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .clamp(1, 3);
                perform_click(webview, x, y, button, click_count)?;
                thread::sleep(Duration::from_millis(120));
                Ok(json!({ "point": { "x": x, "y": y } }))
            }
            "hover" | "move" => {
                let (x, y) = act_point(webview, message)?;
                let params = json!({
                    "type": "mouseMoved",
                    "x": x,
                    "y": y,
                    "button": "none",
                    "clickCount": 0,
                });
                call_cdp(webview, "Input.dispatchMouseEvent", &cdp_params(params))
                    .ok_or_else(|| "CDP mouseMoved failed".to_string())?;
                Ok(json!({ "point": { "x": x, "y": y } }))
            }
            "scroll" => {
                let (x, y) = act_point(webview, message).unwrap_or((1.0, 1.0));
                let delta_x = message
                    .get("deltaX")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let delta_y = message
                    .get("deltaY")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                if !delta_x.is_finite() || !delta_y.is_finite() {
                    return Err("Invalid scroll delta".to_string());
                }
                let params = json!({
                    "type": "mouseWheel",
                    "x": x,
                    "y": y,
                    "deltaX": delta_x,
                    "deltaY": delta_y,
                    "button": "none",
                    "clickCount": 0,
                });
                call_cdp(webview, "Input.dispatchMouseEvent", &cdp_params(params))
                    .ok_or_else(|| "CDP mouseWheel failed".to_string())?;
                thread::sleep(Duration::from_millis(150));
                Ok(json!({ "deltaX": delta_x, "deltaY": delta_y }))
            }
            "type" => {
                let text = message
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "A text value is required".to_string())?;
                if text.len() > 20_000 {
                    return Err("Text value is too large".to_string());
                }
                if message.get("elementRef").and_then(Value::as_str).is_some()
                    || finite_point(message).is_some()
                {
                    let (x, y) = act_point(webview, message)?;
                    perform_click(webview, x, y, "left", 1)?;
                    thread::sleep(Duration::from_millis(80));
                }
                let params = json!({ "text": text });
                call_cdp(webview, "Input.insertText", &cdp_params(params))
                    .ok_or_else(|| "CDP insertText failed".to_string())?;
                thread::sleep(Duration::from_millis(120));
                Ok(json!({ "length": text.chars().count() }))
            }
            "key" | "press" => {
                let key = message
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "A key value is required".to_string())?;
                let Some((code, virtual_key, text)) = key_definition(key) else {
                    return Err(format!("Unsupported key: {key}"));
                };
                let key_down = json!({
                    "type": "keyDown",
                    "key": key,
                    "code": code,
                    "windowsVirtualKeyCode": virtual_key,
                    "nativeVirtualKeyCode": virtual_key,
                    "text": text,
                });
                call_cdp(webview, "Input.dispatchKeyEvent", &cdp_params(key_down))
                    .ok_or_else(|| format!("CDP keyDown {key} failed"))?;
                let key_up = json!({
                    "type": "keyUp",
                    "key": key,
                    "code": code,
                    "windowsVirtualKeyCode": virtual_key,
                    "nativeVirtualKeyCode": virtual_key,
                });
                call_cdp(webview, "Input.dispatchKeyEvent", &cdp_params(key_up))
                    .ok_or_else(|| format!("CDP keyUp {key} failed"))?;
                thread::sleep(Duration::from_millis(120));
                Ok(json!({ "key": key }))
            }
            "wait" => {
                let timeout = message
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(5_000)
                    .clamp(0, 30_000);
                if let Some(selector) = message.get("selector").and_then(Value::as_str) {
                    if selector.len() > 500 {
                        return Err("Selector is too long".to_string());
                    }
                    if !wait_for_selector(webview, selector, timeout) {
                        return Err(format!("Timed out waiting for selector: {selector}"));
                    }
                    Ok(json!({ "selector": selector }))
                } else {
                    thread::sleep(Duration::from_millis(timeout));
                    Ok(json!({ "waitedMs": timeout }))
                }
            }
            "screenshot" => {
                let data = capture_png(webview)
                    .ok_or_else(|| "Native browser screenshot failed".to_string())?;
                Ok(json!({ "dataUrl": format!("data:image/png;base64,{data}") }))
            }
            "snapshot" => snapshot(webview).map(|reply| json!({ "snapshot": reply })),
            "elements" => elements(webview).map(|reply| json!({ "elements": reply })),
            "navigate" => {
                let url = message
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "A URL is required".to_string())?;
                if !valid_url(url) {
                    return Err("Invalid URL".to_string());
                }
                let parsed = Url::parse(url).map_err(|_| "Invalid URL".to_string())?;
                webview
                    .navigate(parsed)
                    .map_err(|error| format!("Could not navigate: {error}"))?;
                Ok(json!({ "url": url }))
            }
            "back" | "forward" | "reload" | "stop" => {
                let result: Option<Result<(), String>> = match action {
                    "back" => run_on_core(webview, |core| unsafe { core.GoBack() })
                        .map(|result| result.map_err(|error| error.to_string())),
                    "forward" => run_on_core(webview, |core| unsafe { core.GoForward() })
                        .map(|result| result.map_err(|error| error.to_string())),
                    "reload" => Some(webview.reload().map_err(|error| error.to_string())),
                    _ => run_on_core(webview, |core| unsafe { core.Stop() })
                        .map(|result| result.map_err(|error| error.to_string())),
                };
                match result {
                    Some(Ok(())) => Ok(json!({})),
                    Some(Err(error)) => {
                        Err(format!("Native browser command failed: {error}"))
                    }
                    None => Err("Native browser tab is unavailable".to_string()),
                }
            }
            _ => Err(format!("Unsupported action: {action}")),
        }
    }

    fn execute_script(webview: &Webview, script: String) -> Option<String> {
        let (sender, receiver) = mpsc::channel();
        let javascript = HSTRING::from(script);
        webview
            .with_webview(move |platform| {
                let core = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => core,
                    Err(_) => {
                        let _ = sender.send(None);
                        return;
                    }
                };
                let handler_sender = sender.clone();
                let handler =
                    ExecuteScriptCompletedHandler::create(Box::new(move |_error, result| {
                        let _ = handler_sender.send(Some(result));
                        Ok(())
                    }));
                if unsafe { core.ExecuteScript(&javascript, &handler) }.is_err() {
                    let _ = sender.send(None);
                }
            })
            .ok()?;
        receiver.recv_timeout(Duration::from_secs(10)).ok().flatten()
    }

    fn capture_png(webview: &Webview) -> Option<String> {
        let (sender, receiver) = mpsc::channel();
        webview
            .with_webview(move |platform| {
                let core = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => core,
                    Err(_) => {
                        let _ = sender.send(None);
                        return;
                    }
                };
                let handler_sender = sender.clone();
                let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
                    move |_error, result| {
                        let data = serde_json::from_str::<Value>(&result)
                            .ok()
                            .and_then(|value| {
                                value.get("data").and_then(Value::as_str).map(str::to_string)
                            });
                        let _ = handler_sender.send(data);
                        Ok(())
                    },
                ));
                let method = HSTRING::from("Page.captureScreenshot");
                let parameters = HSTRING::from("{\"format\":\"png\"}");
                if unsafe { core.CallDevToolsProtocolMethod(&method, &parameters, &handler) }
                    .is_err()
                {
                    let _ = sender.send(None);
                }
            })
            .ok()?;
        receiver.recv_timeout(Duration::from_secs(10)).ok().flatten()
    }

    fn run_on_core<T, F>(webview: &Webview, action: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce(&ICoreWebView2) -> T + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        webview
            .with_webview(move |platform| {
                let result = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => Some(action(&core)),
                    Err(_) => None,
                };
                let _ = sender.send(result);
            })
            .ok()?;
        receiver.recv_timeout(Duration::from_secs(10)).ok().flatten()
    }

    fn post_to_dashboard(app: &AppHandle, message: &Value) {
        let Ok(payload) = serde_json::to_string(message) else {
            return;
        };
        let Some(webview) = app.get_webview("main") else {
            return;
        };
        let _ = webview.with_webview(move |platform| {
            let Ok(core) = (unsafe { platform.controller().CoreWebView2() }) else {
                return;
            };
            let payload = HSTRING::from(payload);
            let _ = unsafe { core.PostWebMessageAsJson(&payload) };
        });
    }

    fn parse_rect(value: &Value) -> Option<Rect> {
        let rect = Rect {
            x: value.get("x")?.as_f64()?,
            y: value.get("y")?.as_f64()?,
            width: value.get("width")?.as_f64()?,
            height: value.get("height")?.as_f64()?,
        };
        if rect.x.is_finite()
            && rect.y.is_finite()
            && rect.width.is_finite()
            && rect.height.is_finite()
            && rect.width >= 0.0
            && rect.height >= 0.0
        {
            Some(rect)
        } else {
            None
        }
    }

    fn nonempty(value: Option<&Value>) -> Option<String> {
        let value = value?.as_str()?;
        if value.is_empty() || value.trim() != value {
            return None;
        }
        Some(value.to_string())
    }

    fn valid_url(value: &str) -> bool {
        sanitize_url(value).is_some()
    }

    /// Search target used when a "URL" is really autolinked body text.
    const SEARCH_TEMPLATE_FALLBACK: &str = "https://www.baidu.com/s?wd={query}";
    /// Optional `{query}` template that overrides the built-in search target.
    const SEARCH_TEMPLATE_ENV: &str = "STARSHIP_BROWSER_SEARCH_URL";

    fn sanitize_url(value: &str) -> Option<String> {
        if value == "about:blank" {
            return Some(value.to_string());
        }
        let parsed = Url::parse(value).ok()?;
        match parsed.scheme() {
            "http" | "https" => {}
            _ => return None,
        }
        Some(search_target_for_bare_idn(&parsed).unwrap_or_else(|| value.to_string()))
    }

    /// The official dashboard autolinks bare words into domains, and for
    /// non-ASCII text that yields a punycoded single-label host such as
    /// `https://xn--fiz18l7ql/` (the label for `财联社`). A lone punycode label
    /// is never a registrable domain, so loading it can only end in a DNS error
    /// page. Decode the label back to the text the user actually wrote and
    /// search for it, which is what a browser does with input that is not an
    /// address.
    fn search_target_for_bare_idn(parsed: &Url) -> Option<String> {
        if parsed.port().is_some() {
            return None;
        }
        let host = parsed.host_str()?.trim_matches(['[', ']']).to_string();
        if host.is_empty()
            || host.contains('.')
            || host.eq_ignore_ascii_case("localhost")
            || !host.to_ascii_lowercase().starts_with("xn--")
        {
            return None;
        }
        let (decoded, status) = idna::domain_to_unicode(&host);
        if status.is_err() {
            return None;
        }
        let query = decoded.trim().to_string();
        if query.is_empty() || query.eq_ignore_ascii_case(&host) {
            return None;
        }
        let target = search_template().replace("{query}", &percent_encode_query(&query));
        let url = Url::parse(&target).ok()?;
        bridge_log(&format!(
            "search fallback host={host} query=\"{query}\" url={url}"
        ));
        Some(url.to_string())
    }

    fn search_template() -> String {
        let configured = std::env::var(SEARCH_TEMPLATE_ENV).unwrap_or_default();
        if configured.contains("{query}") {
            configured
        } else {
            SEARCH_TEMPLATE_FALLBACK.to_string()
        }
    }

    /// Percent-encodes a search term for a query component so the template stays
    /// valid even when the decoded text contains `&`, `#`, or spaces.
    fn percent_encode_query(value: &str) -> String {
        let mut encoded = String::new();
        for byte in value.as_bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    encoded.push(*byte as char)
                }
                _ => encoded.push_str(&format!("%{byte:02X}")),
            }
        }
        encoded
    }

    fn sanitize_label(id: &str) -> String {
        id.chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                    character
                } else {
                    '-'
                }
            })
            .collect()
    }

    fn invalid_request() -> Value {
        json!({ "ok": false, "error": "Invalid native browser request" })
    }

    fn unknown_tab() -> Value {
        json!({ "ok": false, "error": "Unknown native browser tab" })
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::{install, NativeBrowserState, INIT_SCRIPT};

#[cfg(not(target_os = "windows"))]
pub const INIT_SCRIPT: &str = "";

#[cfg(not(target_os = "windows"))]
#[derive(Default)]
pub struct NativeBrowserState;

#[cfg(not(target_os = "windows"))]
pub fn install(_app: tauri::AppHandle) {}
