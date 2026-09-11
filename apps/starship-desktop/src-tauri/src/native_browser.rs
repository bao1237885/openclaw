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
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2, ICoreWebView2Controller};
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2ContentLoadingEventArgs, ICoreWebView2NavigationCompletedEventArgs,
        ICoreWebView2NavigationStartingEventArgs, ICoreWebView2NewWindowRequestedEventArgs,
        ICoreWebView2ProcessFailedEventArgs, ICoreWebView2SourceChangedEventArgs,
        ICoreWebView2WebMessageReceivedEventArgs,
    };
    use windows::core::{HSTRING, BOOL, PWSTR};
    use windows::core::IUnknown;

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
  // A native child WebView2 always paints above the dashboard's HTML, so any
  // chrome that opens *over* the stage - the official panel-type dropdown, the
  // shell's own menus, dialogs - would otherwise be covered by the live page.
  // Report that condition with the probe so the shell can step the native view
  // aside while the overlay is up.
  var OVERLAY_SELECTORS = [
    "wa-dropdown[open]",
    "wa-dialog[open]",
    ".starship-extras__menu",
    ".starship-newtab__menu",
    "[data-starship-overlay]",
  ];
  // Web Awesome keeps the floating surface in the host's shadow root
  // (`wa-dropdown > wa-popup > #menu`, `wa-dialog > dialog`) and the host
  // element itself is only the ~28px trigger button. Measuring the host would
  // report a rect that never reaches the stage, so every candidate is resolved
  // to the surface that actually paints on screen.
  var OVERLAY_SURFACE_SELECTOR =
    '[part~="menu"], [part~="popup"], [part~="dialog"], dialog, wa-popup';
  function overlaySurfaceRects(node, rects) {
    var surfaceStyle = window.getComputedStyle(node);
    if (
      surfaceStyle.display === "none" ||
      surfaceStyle.visibility === "hidden" ||
      Number(surfaceStyle.opacity || "1") <= 0
    ) {
      return;
    }
    var surfaceRect = node.getBoundingClientRect();
    if (surfaceRect.width > 1 && surfaceRect.height > 1) { rects.push(surfaceRect); }
  }
  function visibleOverlayRects() {
    var rects = [];
    for (var s = 0; s < OVERLAY_SELECTORS.length; s += 1) {
      var nodes = document.querySelectorAll(OVERLAY_SELECTORS[s]);
      for (var n = 0; n < nodes.length; n += 1) {
        var node = nodes[n];
        if (node.hidden) { continue; }
        // The host matters too: shell-owned menus are plain elements whose own
        // rect is the whole popup.
        overlaySurfaceRects(node, rects);
        var shadow = node.shadowRoot;
        if (!shadow) { continue; }
        var surfaces = shadow.querySelectorAll(OVERLAY_SURFACE_SELECTOR);
        for (var i = 0; i < surfaces.length; i += 1) {
          overlaySurfaceRects(surfaces[i], rects);
        }
      }
    }
    // 星舰自己的两个菜单挂在浏览器面板的 shadow root 里，document.querySelectorAll
    // 穿不透那层边界：漏报就等于「没有遮挡」，壳层不会把原生 WebView2 让开，菜单
    // 下半截会被网页盖住——而遮挡判断偏偏只在菜单真正盖到网页时才有意义。
    shadowOverlayRects(rects);
    return rects;
  }
  // Kept separate from the loop above because it has to reach into a different
  // tree (panel shadow roots) rather than the light DOM.
  function shadowOverlayRects(rects) {
    var panels = document.querySelectorAll(PANEL_SELECTOR);
    for (var index = 0; index < panels.length; index += 1) {
      var root = panelRoot(panels[index]);
      if (!root) { continue; }
      var menus = root.querySelectorAll(STARSHIP_MENU_SELECTOR);
      for (var item = 0; item < menus.length; item += 1) {
        if (menus[item].hidden) { continue; }
        overlaySurfaceRects(menus[item], rects);
      }
    }
  }
  function overlaysCover(rect) {
    var overlays = visibleOverlayRects();
    for (var index = 0; index < overlays.length; index += 1) {
      var overlay = overlays[index];
      if (overlay.right <= rect.left || overlay.left >= rect.right) { continue; }
      if (overlay.bottom <= rect.top || overlay.top >= rect.bottom) { continue; }
      return true;
    }
    return false;
  }
  function publishShellProbe(force) {
    var probe = { visible: false, tabId: null, rect: null };
    var measurement = livePanelMeasurement();
    trackProbeStage(measurement ? measurement.stage : null);
    if (measurement) {
      probe = {
        visible: true,
        tabId: measurement.tabId,
        scope: measurement.scope,
        occluded: overlaysCover(measurement.rect),
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
  // The official "+" trigger ships a `title` attribute ("添加侧边栏面板标签页").
  // Chromium paints that native tooltip on its own layer *above* the dropdown
  // the same button just opened, so the first item ("审阅") disappears behind a
  // label that belongs to the control underneath. A tooltip cannot be targeted
  // by CSS, so the attribute is lifted for as long as the menu stays open and
  // restored on close; the accessible name survives because the official
  // trigger also carries `aria-label`.
  function syncOpenDropdownTooltips() {
    var dropdowns = document.querySelectorAll("wa-dropdown");
    for (var d = 0; d < dropdowns.length; d += 1) {
      var trigger = dropdowns[d].querySelector('[slot="trigger"]');
      if (!trigger) { continue; }
      if (dropdowns[d].hasAttribute("open")) {
        if (trigger.hasAttribute("title")) {
          trigger.setAttribute("data-starship-title", trigger.getAttribute("title"));
          trigger.removeAttribute("title");
        }
      } else if (trigger.hasAttribute("data-starship-title")) {
        trigger.setAttribute("title", trigger.getAttribute("data-starship-title"));
        trigger.removeAttribute("data-starship-title");
      }
    }
  }
  window.setInterval(function () {
    publishShellProbe(false);
    syncOpenDropdownTooltips();
  }, PROBE_POLL_MS);
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
    function () {
      startProbeBurst();
      syncOpenDropdownTooltips();
    },
    true,
  );
  // The tooltip is armed by hover, i.e. before `open` flips, so strip it from
  // the pressed dropdown trigger as well and let the periodic sync restore it
  // when the press did not turn into an open menu.
  document.addEventListener(
    "pointerdown",
    function (event) {
      var node = event.target;
      while (node && node !== document) {
        if (
          typeof node.hasAttribute === "function" &&
          node.getAttribute("slot") === "trigger" &&
          node.hasAttribute("title")
        ) {
          node.setAttribute("data-starship-title", node.getAttribute("title"));
          node.removeAttribute("title");
          return;
        }
        node = node.parentNode;
      }
    },
    true,
  );
  // Menus also open without a click: the panel-type dropdown answers keyboard
  // shortcuts, and dismissals can reveal another surface in the same frame.
  // Watching the reflected `open` attribute keeps those paths as fast as a
  // click, so the native view steps aside before the menu paints on top of it.
  //
  // 这段必须能等：注入脚本在 document_start 执行，那一刻 `document.documentElement`
  // 还是 null，而 `observe(null)` 会直接抛 TypeError。抛在这里会**静默掐断整段注入脚本**
  // 后面所有内容（驱动 API、Codex parity 样式层、移动 "+" 的逻辑都没了），
  // 且页面上看不出任何错误——所以既不能同步调用，也不能只靠 try/catch 掩盖。
  function watchOverlayOpens() {
    if (typeof MutationObserver !== "function") { return; }
    var root = document.documentElement;
    if (!root) {
      window.setTimeout(watchOverlayOpens, 25);
      return;
    }
    var overlayOpenObserver = new MutationObserver(function (records) {
      for (var index = 0; index < records.length; index += 1) {
        if (records[index].attributeName === "open") {
          startProbeBurst();
          syncOpenDropdownTooltips();
          return;
        }
      }
    });
    overlayOpenObserver.observe(root, {
      subtree: true,
      attributes: true,
      attributeFilter: ["open"],
    });
  }
  watchOverlayOpens();
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

  // -------------------------------------------------------------------
  // Starship Codex-parity chrome layer (runtime only, shell-owned).
  //
  // The official panel renders inside an *open* shadow root, so the shell can
  // restyle and extend its chrome from inside the page without patching the
  // npm bundle. Everything below is additive and guarded: when a future
  // OpenClaw release renames a `.bp-*` hook the layer stops matching instead
  // of breaking the panel, and `localStorage.starshipBrowserSkin = "off"`
  // turns it back off without a rebuild.
  // -------------------------------------------------------------------
  var PANEL_SELECTOR = "openclaw-browser-panel";
  var PARITY_CSS = `
/* Values mirror Codex's browser tab row + tool row metrics.
   Two cascade facts drive the shape of this sheet:
   1. the official component declares its styles as Lit element styles, which
      land in shadowRoot.adoptedStyleSheets and are therefore ordered after
      any <style> element appended to the same root, so a rule of equal
      specificity written here loses. Every rule that fights an official
      declaration is scoped through \`:is(...)\` to raise specificity instead of
      relying on document order.
   2. where OpenClaw exposes a styling knob (\`--rail-header-*\`) the official
      value is driven through that knob, so an upstream rework of the rail
      keeps working instead of being overridden. */
.bp-header {
  /* 48px 是官方这一行的原生高度，也是右侧栏 grid 第一行的高度。抬到第一行之后必须
     用同一个高度，标签行才会和左侧聊天头共处一行、两条下边框连成一条线；32px 的
     紧凑值会让右侧那条线比左侧高出一截。 */
  --rail-header-height: 48px;
  --rail-header-padding-start: 8px;
  --rail-header-padding-end: 8px;
  --rail-header-background: transparent;
}
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-header {
  height: 48px;
  min-height: 48px;
  padding: 10px 8px;
  gap: 6px;
  /* 官方用 space-between：只有「标签 + 一个按钮」时，那个按钮会被顶到最右，
     离标签隔一整个面板的距离。Codex 的标签行是左对齐的「标签 +」，所以这里
     改成靠左堆叠，最右侧的外开按钮用 margin-left:auto 自己顶过去。 */
  justify-content: flex-start;
  background: transparent;
}
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-toolbar {
  min-height: 36px;
  padding: 4px 8px;
  gap: 2px;
  overflow: hidden;
}
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-toolbar .bp-icon {
  width: 26px;
  height: 26px;
  border-radius: 7px;
}
/* Codex centres the URL pill between the navigation cluster and the trailing
   actions; the official toolbar stretches it edge to edge instead. */
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-toolbar .bp-url {
  flex: 0 1 clamp(200px, 46%, 620px);
  min-width: 96px;
  height: 26px;
  margin-left: auto;
  margin-right: auto;
  border-radius: 13px;
  text-align: center;
}
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-toolbar .bp-url:focus {
  text-align: left;
}
/* The embedded new-tab control is relocated into the tab row (see
   moveNewTabToRail). Upstream declares the bp-icon metrics under the toolbar
   scope only, so a control that leaves the tool row falls back to the
   user-agent button box: a blank rectangle that reads as if the tab row's
   new-tab button were missing. Restate the metrics for the relocated control
   instead of leaving it unstyled. */
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-header .bp-icon {
  display: inline-flex;
  flex: none;
  align-self: center;
  width: 26px;
  height: 26px;
  margin-left: 4px;
  align-items: center;
  justify-content: center;
  padding: 0;
  border: 0;
  border-radius: 7px;
  background: transparent;
  color: var(--muted, #8a919e);
}
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-header .bp-icon:hover,
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-header .bp-icon:focus-visible {
  background: color-mix(in srgb, var(--text, #d7dae0) 10%, transparent);
  color: var(--text, #d7dae0);
}
/* 标签行上的两个搬迁控件各归各位：新建标签页紧贴标签胶囊（上面那条 .bp-icon
   规则已经给了 4px 左边距），外开按钮吃满剩余空间、停在最右。分开写是因为
   两者共用的 .bp-icon 规则只能给一个 margin-left。 */
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-header .bp-icon[data-starship-open-external] {
  margin-left: auto;
}
/* 顶部那一行的官方「+」现在是星舰菜单的入口（见 installHostAddMenu）。标签行里
   搬上来的这个「新建标签页」按钮就退回成纯程序化入口（菜单里的「标签页」点它），
   不再露脸——否则屏幕上并排站着两个「+」，正是用户报的重复。 */
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-header .bp-icon[data-starship-new-tab] {
  display: none;
}
.bp--embedded {
  overflow: hidden;
}
:is(.bp--embedded, .bp--right, .bp--bottom) .bp-viewport {
  overflow-x: hidden;
  overflow-y: auto;
  scrollbar-width: thin;
}
.starship-extras__toggle {
  order: 99;
}
/* 两个菜单共用同一套盒式样式，位置一律由 placeMenu() 按各自的触发按钮算出来。
   早先这里写死 40px / 8px 两个偏移，把工具菜单钉在面板右上角——而「+」正好
   就在那一带，于是 ⋮ 的菜单看起来像「+」弹出的，用户以为新建按钮丢了菜单。
   位置交给 JS 之后，每个菜单只跟着自己的按钮走。 */
.starship-extras__menu,
.starship-newtab__menu {
  position: absolute;
  z-index: 40;
  min-width: 208px;
  padding: 4px;
  border: 1px solid var(--border, #262b34);
  border-radius: 10px;
  background: var(--bg, #0e1015);
  box-shadow: 0 6px 24px rgba(0, 0, 0, 0.35);
  font: inherit;
}
.starship-extras__menu button,
.starship-newtab__menu button {
  display: flex;
  width: 100%;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 6px 10px;
  border: 0;
  border-radius: 7px;
  background: transparent;
  color: var(--text, #d7dae0);
  font: inherit;
  font-size: 12.5px;
  text-align: left;
}
.starship-extras__menu button:hover,
.starship-newtab__menu button:hover {
  background: color-mix(in srgb, var(--text, #d7dae0) 10%, transparent);
}
/* 菜单项三件套：图标 / 名称 / 快捷键。图标只约束尺寸，颜色交给 currentColor，
   这样官方把图标换成 fill 或 stroke 画法都跟着走；名称占满剩余宽度并左对齐，
   快捷键贴右边。名称这条必须盖住下面那条通用 span 规则，否则又变回灰字。 */
.starship-extras__menu button .starship-extras__icon,
.starship-newtab__menu button .starship-extras__icon {
  display: inline-flex;
  flex: none;
  align-items: center;
  justify-content: center;
  width: 16px;
  height: 16px;
  color: var(--muted, #8a919e);
  font-size: 0;
}
.starship-extras__icon > svg {
  width: 16px;
  height: 16px;
  flex: none;
}
.starship-extras__menu button .starship-extras__label,
.starship-newtab__menu button .starship-extras__label {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  color: inherit;
  font-size: 12.5px;
  text-align: left;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.starship-extras__menu button .starship-extras__hint,
.starship-newtab__menu button .starship-extras__hint {
  flex: none;
  color: var(--muted, #8a919e);
  font-size: 11.5px;
}
/* 菜单分两级：上面是官方面板标签（随官方增删，现取现用），下面是星舰自己的
   工具项。两者语义不同，靠组头 + 分割线分开，避免读成一串并列命令。 */
.starship-extras__menu[hidden],
.starship-newtab__menu[hidden] {
  display: none;
}
.starship-extras__group {
  padding: 5px 10px 2px;
  color: var(--muted, #8a919e);
  font-size: 10.5px;
  font-weight: 600;
  letter-spacing: 0.06em;
}
.starship-extras__divider {
  height: 1px;
  margin: 4px 6px;
  background: var(--border, #262b34);
}
.starship-findbar {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 5px 8px;
  border-bottom: 1px solid var(--border, #262b34);
}
.starship-findbar input {
  flex: 1;
  min-width: 0;
  height: 26px;
  padding: 0 10px;
  border: 1px solid var(--border, #262b34);
  border-radius: 13px;
  background: var(--bg, #0e1015);
  color: var(--text, #d7dae0);
  font: inherit;
  font-size: 12.5px;
  outline: none;
}
.starship-findbar__count {
  color: var(--muted, #8a919e);
  font-size: 11.5px;
  white-space: nowrap;
}
`;
  // 官方面板类型行（`.side-panel__header`）就是「审阅 / 浏览器 / +」那一整行。星舰
  // 一度把它收起来、再把面板顶到 grid 第一行，好对齐 Codex 的两行结构；代价是官方
  // 「+」里那份面板类型清单（审阅 / 终端 / 浏览器 / 文件 / 侧边聊天 / 任务 / 桌面 /
  // 仪表盘）跟着那一行一起从界面上消失，用户没有第二条路把它叫回来。用户明确否掉了
  // 这台取舍：那一行原样保留。星舰改成**镜像**——自己的「+」菜单就地读官方那份清单，
  // 官方加一个面板类型，星舰的「+」自动多一项，不需要硬编码任何一条。
  var HOST_RAIL_SELECTOR = '[data-region-header="side"]';
  var HOST_RAIL_PANEL_TAB_SELECTOR = ".tabstrip-tab";
  // 顶部那一行里的官方「+」。星舰自己的「+」是官方工具行里那个「新建标签页」
  // （`[data-new-tab-action]`），早先被搬到网页标签行，于是屏幕上同时站着两个
  // 「+」：官方的在建面板、星舰的在建标签页。用户看到的就是重复。
  //
  // 定下来的形态是 Codex 的形态：**顶部只留一个「+」**。做法是把官方那个
  // 「+」接过来当星舰菜单的入口——星舰菜单本来就是把官方那份面板类型清单
  // 现读一遍再补一条「标签页」，所以挂到哪个按钮上，功能都不缺；反过来把
  // 星舰那个「+」收进 DOM（`display:none`）只留官方那个按钮站在原位，位置、
  // 尺寸、悬停都不用重新对齐，官方那一行也不动。
  var HOST_ADD_TRIGGER_SELECTOR = "button.side-panel-type-menu__trigger";
  var OFFICIAL_ADD_DROPDOWN_SELECTOR = "wa-dropdown";
  var OFFICIAL_ADD_ITEM_SELECTOR = "wa-dropdown-item";
  var OFFICIAL_ADD_LABEL_SELECTOR = ".side-panel-type-option__label";
  var OFFICIAL_ADD_SHORTCUT_SELECTOR = ".side-panel-type-option__shortcut";
  var OFFICIAL_ADD_ICON_SELECTOR = ".side-panel-type-option__icon";
  var CLOSE_GLYPH =
    '<svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" ' +
    'stroke-width="1.5" stroke-linecap="round"><path d="M4 4l8 8M12 4l-8 8"/></svg>';
  // 星舰在面板上叠的浮层（菜单 / 查找条）都带这个前缀，遮挡探测和「点外面关掉」
  // 都需要一次拿到全部，省得每加一个浮层就漏改一处。
  var STARSHIP_MENU_SELECTOR = ".starship-extras__menu, .starship-newtab__menu";
  var STARSHIP_OVERLAY_SELECTOR = STARSHIP_MENU_SELECTOR + ", .starship-extras__toggle";
  var STARSHIP_NEW_TAB_SELECTOR = "[data-starship-new-tab]";
  // 点这些控件不算「点外面」：菜单的触发按钮、菜单本体，以及顶部那个被星链接管
  // 成菜单入口的官方「+」。少了最后一条，点「+」会先把菜单关掉再打开，看着像点
  // 不动——菜单永远关不掉。
  var STARSHIP_MENU_ANCHOR_SELECTOR =
    STARSHIP_OVERLAY_SELECTOR + ", " + STARSHIP_NEW_TAB_SELECTOR + ", " + HOST_ADD_TRIGGER_SELECTOR;
  var MENU_GLYPH =
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor">' +
    '<circle cx="8" cy="3" r="1.3"></circle><circle cx="8" cy="8" r="1.3"></circle>' +
    '<circle cx="8" cy="13" r="1.3"></circle></svg>';
  function parityEnabled() {
    try {
      return window.localStorage.getItem("starshipBrowserSkin") !== "off";
    } catch (error) {
      return true;
    }
  }
  function panelRoot(panel) {
    try {
      return panel.shadowRoot || null;
    } catch (error) {
      return null;
    }
  }
  function panelTabId(panel) {
    var controller = panel.browserPanelController;
    var tabId = controller && controller.activeTargetId;
    return typeof tabId === "string" && tabId ? tabId : null;
  }
  // The official component adopts its own stylesheet list, and adopted sheets
  // outrank a plain <style> element in the same shadow root. Appending our
  // sheet after theirs means an equal-specificity rule written here still wins
  // on a future OpenClaw release that changes the rail metrics. Lit re-assigns
  // the list when a cached chat pane re-mounts, so this re-checks on every scan
  // instead of assuming a one-time injection survives.
  var paritySheets = new WeakMap();
  // Codex 的标签行是「标签胶囊 + 紧挨着的 +」，右上角另有一个「在外部打开」。
  // 官方把两个新建/外开按钮都放在工具行，于是标签行是一条空带，工具行尾部的
  // 图标堆又太宽。URL 输入框是天然分界：它前面的是「新建标签页」，后面的是
  // 「在外部打开当前页」。两个都上移到标签行，各带各的标记。
  //
  // 官方面板是 Lit 渲染的：重渲染会在工具行里造出**全新**的按钮，而早先搬上去
  // 的那份还留在标签行。两份叠在同一位置互相覆盖，就是「新建标签按钮看起来
  // 消失」的成因。所以给搬上去的节点打标记，并删掉所有不是「当前工具行里那个
  // 控件」的带标记节点。
  var RAIL_NEW_TAB_TAG = "data-starship-new-tab";
  var RAIL_OPEN_EXTERNAL_TAG = "data-starship-open-external";
  function settleRailControl(header, tag, control) {
    if (!control) { return; }
    var settled = header.querySelectorAll("[" + tag + "]");
    for (var stale = 0; stale < settled.length; stale += 1) {
      if (settled[stale] !== control) { settled[stale].remove(); }
    }
    control.setAttribute(tag, "1");
    if (control.parentNode !== header) { header.appendChild(control); }
  }
  function moveNewTabToRail(root, toolbar) {
    var header = root.querySelector(".bp-header");
    if (!header) { return; }
    var url = toolbar.querySelector(".bp-url");
    var candidates = toolbar.querySelectorAll("[data-new-tab-action]");
    var control = null;
    var external = null;
    for (var index = 0; index < candidates.length; index += 1) {
      var button = candidates[index];
      var beforeUrl = !url || (url.compareDocumentPosition(button) & 2) === 2;
      if (beforeUrl) {
        if (!control) { control = button; }
      } else if (!external) {
        external = button;
      }
    }
    settleRailControl(header, RAIL_NEW_TAB_TAG, control);
    settleRailControl(header, RAIL_OPEN_EXTERNAL_TAG, external);
    // 官方重渲染可能只换掉其中一个，顺序得显式钉住，否则「+」会排到外开按钮后面。
    if (
      control && external &&
      control.parentNode === header && external.parentNode === header &&
      external.previousElementSibling !== control
    ) {
      header.insertBefore(control, external);
    }
  }
  function adoptParitySheet(root) {
    if (typeof CSSStyleSheet !== "function" || !root.adoptedStyleSheets) { return; }
    try {
      var sheet = paritySheets.get(root);
      if (!sheet) {
        sheet = new CSSStyleSheet();
        sheet.replaceSync(PARITY_CSS);
        paritySheets.set(root, sheet);
      }
      if (root.adoptedStyleSheets.indexOf(sheet) === -1) {
        root.adoptedStyleSheets = root.adoptedStyleSheets.concat([sheet]);
      }
    } catch (error) {
      /* The <style> element above already carries the same rules. */
    }
  }
  function actOnPanel(panel, action, extra) {
    var tabId = panelTabId(panel);
    if (!tabId) {
      return Promise.resolve({ ok: false, error: "No browser tab is selected" });
    }
    var payload = { type: "act", action: action, tabId: tabId };
    if (extra) {
      for (var key in extra) {
        if (Object.prototype.hasOwnProperty.call(extra, key)) { payload[key] = extra[key]; }
      }
    }
    return postMessage(payload);
  }
  function livePanelElement() {
    var panels = document.querySelectorAll(PANEL_SELECTOR);
    for (var index = 0; index < panels.length; index += 1) {
      var panel = panels[index];
      if (!panelRoot(panel)) { continue; }
      var pane = typeof panel.closest === "function"
        ? panel.closest("openclaw-chat-pane")
        : null;
      if (pane && !pane.classList.contains(LIVE_PANE_CLASS)) { continue; }
      var visible =
        typeof panel.checkVisibility === "function"
          ? panel.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })
          : panel.offsetParent !== null;
      if (visible) { return panel; }
    }
    return null;
  }
  function stopFind(panel, bar) {
    if (bar && bar.parentNode) { bar.parentNode.removeChild(bar); }
    actOnPanel(panel, "findStop", {});
  }
  function openFindBar(panel, root) {
    var existing = root.querySelector(".starship-findbar");
    if (existing) {
      var previousInput = existing.querySelector("input");
      if (previousInput) { previousInput.focus(); previousInput.select(); }
      return;
    }
    var toolbar = root.querySelector(".bp-toolbar");
    if (!toolbar) { return; }
    var bar = document.createElement("div");
    bar.className = "starship-findbar";
    var input = document.createElement("input");
    input.type = "text";
    input.spellcheck = false;
    input.setAttribute("placeholder", "\u5728\u9875\u9762\u4e2d\u67e5\u627e");
    var count = document.createElement("span");
    count.className = "starship-findbar__count";
    var close = document.createElement("button");
    close.className = "bp-icon";
    close.type = "button";
    close.title = "\u5173\u95ed\u67e5\u627e";
    close.setAttribute("aria-label", "\u5173\u95ed\u67e5\u627e");
    close.innerHTML = CLOSE_GLYPH;
    close.addEventListener("click", function () { stopFind(panel, bar); });
    function run(forward, findNext) {
      var text = input.value;
      if (!text) { count.textContent = ""; return; }
      actOnPanel(panel, "find", {
        text: text,
        forward: forward,
        findNext: findNext,
      }).then(function (reply) {
        var detail = reply && reply.detail && reply.detail.find;
        if (detail && typeof detail.matches === "number") {
          count.textContent =
            detail.matches > 0
              ? (detail.activeMatchOrdinal || 1) + "/" + detail.matches
              : "0/0";
        }
      });
    }
    input.addEventListener("input", function () { run(true, false); });
    input.addEventListener("keydown", function (event) {
      if (event.key === "Enter") {
        event.preventDefault();
        run(!event.shiftKey, true);
      } else if (event.key === "Escape") {
        event.preventDefault();
        stopFind(panel, bar);
      }
    });
    bar.appendChild(input);
    bar.appendChild(count);
    bar.appendChild(close);
    toolbar.insertAdjacentElement("afterend", bar);
    input.focus();
  }
  // 官方「+」那份清单才是用户认的「新建什么」。星舰的「+」把它整份搬过来，
  // 但不抄文案、不抄快捷键、不抄图标：条目在菜单每次打开时现读官方 DOM（图标节点
  // 深拷贝一份），点击转发给官方那个 item 本身。官方改文案 / 加快捷键 / 换顺序 /
  // 增删面板类型，星舰菜单自动跟随，不需要这边维护第二份面板清单。
  function sidePanelTabName(tab) {
    var label = tab.querySelector(".tabstrip-tab__label");
    var name = label ? label.textContent : "";
    if (!name) { name = tab.getAttribute("aria-label") || ""; }
    return (name || "").replace(/\s+/g, " ").trim();
  }
  function sidePanelTabCurrent(tab) {
    return tab.hasAttribute("active") || tab.getAttribute("aria-selected") === "true";
  }
  // 官方面板类型清单（审阅 / 终端 / 浏览器 / 文件 / 侧边聊天 / 任务 / 桌面 / 仪表盘）。
  // 每次都现取：官方那份菜单是渲染出来的，面板开合会改变「已打开」标记，缓存一份
  // 就会显示假的。官方换掉内部类名时退回读已开标签，菜单里至少还有东西可点。
  function officialPanelEntries(panel) {
    var entries = [];
    var header = hostRailHeaderFor(panel);
    if (!header) { return entries; }
    var dropdown = header.querySelector(OFFICIAL_ADD_DROPDOWN_SELECTOR);
    var items = dropdown ? dropdown.querySelectorAll(OFFICIAL_ADD_ITEM_SELECTOR) : [];
    for (var index = 0; index < items.length; index += 1) {
      var item = items[index];
      // 触发的那个 button 也在这份列表里（slot="trigger"），它不是面板类型。
      if (item.getAttribute("slot") === "trigger") { continue; }
      var labelNode = item.querySelector(OFFICIAL_ADD_LABEL_SELECTOR);
      var shortcutNode = item.querySelector(OFFICIAL_ADD_SHORTCUT_SELECTOR);
      var iconNode = item.querySelector(OFFICIAL_ADD_ICON_SELECTOR);
      var label = labelNode ? labelNode.textContent : item.textContent;
      label = (label || "").replace(/\s+/g, " ").trim();
      if (!label) { continue; }
      entries.push({
        group: "\u9762\u677f",
        label: label,
        hint: shortcutNode ? shortcutNode.textContent.trim() : "",
        icon: iconNode,
        run: (function (target) {
          return function () { target.click(); };
        })(item),
      });
    }
    if (entries.length > 0) { return entries; }
    var tabs = header.querySelectorAll(HOST_RAIL_PANEL_TAB_SELECTOR);
    for (var fallback = 0; fallback < tabs.length; fallback += 1) {
      var tab = tabs[fallback];
      var tabName = sidePanelTabName(tab);
      if (!tabName) { continue; }
      entries.push({
        group: "\u9762\u677f",
        label: tabName,
        hint: sidePanelTabCurrent(tab) ? "\u5f53\u524d" : "\u5207\u6362",
        run: (function (target) {
          return function () { target.click(); };
        })(tab),
      });
    }
    return entries;
  }
  // 「+」在 Codex 里是「你要新建什么」的菜单入口（侧边聊天 / 浏览器 / 终端）。官方
  // 那个「+」给的就是这份清单，星舰这个「+」给同一份，再补一条「标签页」。
  function newTabEntries(panel, root) {
    var entries = officialPanelEntries(panel);
    entries.push({
      group: "\u65b0\u5efa",
      label: "\u6807\u7b7e\u9875",
      hint: "Ctrl+T",
      run: function () { clickNewTab(root); },
    });
    return entries;
  }
  // 工具菜单只剩工具：面板类型搬去「+」之后，这里再列一遍面板就会有两个入口指向
  // 同一件事，用户反而不知道该点哪个。
  function extrasEntries(panel, root) {
    var entries = [];
    entries.push({
      group: "\u5de5\u5177",
      label: "\u67e5\u627e\u2026",
      hint: "Ctrl+F",
      run: function () { openFindBar(panel, root); },
    });
    entries.push({
      group: "\u5de5\u5177",
      label: "\u653e\u5927",
      hint: "+10%",
      run: function () { actOnPanel(panel, "zoom", { direction: "in" }); },
    });
    entries.push({
      group: "\u5de5\u5177",
      label: "\u7f29\u5c0f",
      hint: "-10%",
      run: function () { actOnPanel(panel, "zoom", { direction: "out" }); },
    });
    entries.push({
      group: "\u5de5\u5177",
      label: "\u91cd\u7f6e\u7f29\u653e",
      hint: "100%",
      run: function () { actOnPanel(panel, "zoom", { direction: "reset" }); },
    });
    entries.push({
      group: "\u5de5\u5177",
      label: "\u5f00\u53d1\u8005\u5de5\u5177",
      hint: "F12",
      run: function () { actOnPanel(panel, "devtools", { mode: "open" }); },
    });
    entries.push({
      group: "\u5de5\u5177",
      label: "\u4e0b\u8f7d\u6587\u4ef6\u5939",
      hint: "Downloads",
      run: function () { actOnPanel(panel, "downloads", { open: true }); },
    });
    return entries;
  }
  function fillMenu(menu, entries) {
    while (menu.firstChild) { menu.removeChild(menu.firstChild); }
    var group = null;
    for (var index = 0; index < entries.length; index += 1) {
      var entry = entries[index];
      if (entry.group && entry.group !== group) {
        if (group !== null) {
          var divider = document.createElement("div");
          divider.className = "starship-extras__divider";
          menu.appendChild(divider);
        }
        group = entry.group;
        var heading = document.createElement("div");
        heading.className = "starship-extras__group";
        heading.textContent = group;
        menu.appendChild(heading);
      }
      var button = document.createElement("button");
      button.type = "button";
      button.setAttribute("role", "menuitem");
      // 官方那份面板类型清单每项都带图标。星舰镜像过来时如果不带上，菜单就变成
      // 一串没有识别点的灰字，和官方那份对不上号。这里直接深拷贝官方 item 里的
      // 图标节点：官方换图标、跟着换，颜色靠 currentColor 继承主题，不写死。
      if (entry.icon && typeof entry.icon.cloneNode === "function") {
        var icon = entry.icon.cloneNode(true);
        icon.setAttribute("class", "starship-extras__icon");
        icon.removeAttribute("slot");
        button.appendChild(icon);
      }
      var label = document.createElement("span");
      label.className = "starship-extras__label";
      label.textContent = entry.label;
      label.style.color = "inherit";
      var hint = document.createElement("span");
      hint.className = "starship-extras__hint";
      hint.textContent = entry.hint;
      button.appendChild(label);
      button.appendChild(hint);
      (function (action) {
        button.addEventListener("click", function (event) {
          event.preventDefault();
          event.stopPropagation();
          menu.hidden = true;
          action();
        });
      })(entry.run);
      menu.appendChild(button);
    }
  }
  function closeMenus(root) {
    var menus = root.querySelectorAll(STARSHIP_MENU_SELECTOR);
    for (var index = 0; index < menus.length; index += 1) {
      menus[index].hidden = true;
    }
  }
  // 菜单按视口坐标算位置，但 left/top 是相对**包含块**写的。包含块不是面板本体：
  // 官方把右侧栏包在 .sidebar-region 里，那一层才是最近的定位祖先，菜单从面板左边缘
  // 起算会整体左移一大截（实测差 756px，直接跑到聊天区中间）。所以先把菜单停在
  // 0/0 读出包含块原点，再折算成相对坐标；两帧内同步完成，不会闪。
  function panelBox(root) {
    var host = root && root.host;
    if (host && typeof host.getBoundingClientRect === "function") {
      return host.getBoundingClientRect();
    }
    return { left: 0, top: 0, right: 0, bottom: 0, width: 0, height: 0 };
  }
  function placeMenu(menu, trigger, root) {
    var rect = trigger.getBoundingClientRect();
    menu.style.left = "0px";
    menu.style.top = "0px";
    var origin = menu.getBoundingClientRect();
    var width = menu.offsetWidth || 208;
    // 每个菜单跟着自己的触发按钮：新建菜单与「+」左对齐，工具菜单与「⋮」右对齐
    // （它是工具行最后一个控件，右对齐才不会顶着面板边缘）。
    var left = trigger.hasAttribute("data-starship-menu-right")
      ? rect.right - width
      : rect.left;
    // 夹的是面板本体而不是包含块：包含块横跨聊天区和右侧栏，按它夹等于没夹。
    var box = panelBox(root);
    var minLeft = box.left + 8;
    var maxLeft = box.right - width - 8;
    if (maxLeft > minLeft) {
      left = Math.max(minLeft, Math.min(maxLeft, left));
    }
    menu.style.left = Math.round(left - origin.left) + "px";
    menu.style.top = Math.round(rect.bottom + 6 - origin.top) + "px";
  }
  function openMenu(menu, trigger, root, entries) {
    closeMenus(root);
    fillMenu(menu, entries);
    menu.hidden = false;
    placeMenu(menu, trigger, root);
  }
  // 官方「+」自己会立刻建一个新标签。菜单里再点「标签页」时要把这次程序化点击
  // 放行，否则会被下面的拦截器当成用户点击又弹一次菜单。
  var newTabBypass = false;
  function clickNewTab(root) {
    var header = root.querySelector(".bp-header");
    var button = header ? header.querySelector(STARSHIP_NEW_TAB_SELECTOR) : null;
    if (!button) { return; }
    newTabBypass = true;
    try {
      button.click();
    } finally {
      newTabBypass = false;
    }
  }
  // 「+」的点击必须拦下来：官方接的是「直接新建」，而 Codex 的「+」是先给菜单。
  // 用捕获阶段挂在标签行上，官方的监听器还没轮到就已经被 stopImmediatePropagation
  // 掐掉，标签行里其它控件（标签胶囊、外开按钮）不受影响。
  function installNewTabMenu(panel, root) {
    var header = root.querySelector(".bp-header");
    if (!header || header.hasAttribute("data-starship-newtab-menu")) { return; }
    header.setAttribute("data-starship-newtab-menu", "1");
    header.addEventListener(
      "click",
      function (event) {
        var node = event.target;
        if (!node || typeof node.closest !== "function") { return; }
        var button = node.closest(STARSHIP_NEW_TAB_SELECTOR);
        if (!button) { return; }
        if (newTabBypass) { return; }
        event.preventDefault();
        event.stopImmediatePropagation();
        var menu = root.querySelector(".starship-newtab__menu");
        if (!menu) { return; }
        if (menu.hidden) {
          openMenu(menu, button, root, newTabEntries(panel, root));
        } else {
          menu.hidden = true;
        }
      },
      true,
    );
  }
  // 悬空菜单是最容易被误读成「界面坏了」的状态：用户点开之后在别处一点，菜单
  // 还挂着。用 pointerdown 而不是 click，是为了在下一个控件响应之前就收掉。
  function installMenuDismiss(panel, root) {
    // ShadowRoot 是 DocumentFragment，没有 setAttribute，标记只能挂在对象上。
    if (root.__starshipMenuDismiss) { return; }
    root.__starshipMenuDismiss = true;
    root.addEventListener(
      "pointerdown",
      function (event) {
        var node = event.target;
        if (node && typeof node.closest === "function" && node.closest(STARSHIP_MENU_ANCHOR_SELECTOR)) {
          return;
        }
        closeMenus(root);
      },
      true,
    );
    root.addEventListener(
      "keydown",
      function (event) {
        if (event.key !== "Escape") { return; }
        // STARSHIP_MENU_SELECTOR 是逗号列表，直接拼 :not() 只会修饰最后一段，
        // 排在前面的那个菜单会被无条件读成「正开着」——Escape 于是在面板里被永久
        // 吞掉：查找栏关不掉，官方面板自己的 Esc 也一起失效。必须用 :is() 把整份
        // 列表收进一个复合选择器里。
        var open = root.querySelector(":is(" + STARSHIP_MENU_SELECTOR + "):not([hidden])");
        if (!open) { return; }
        closeMenus(root);
        event.preventDefault();
        event.stopPropagation();
      },
      true,
    );
  }
  // 菜单挂在面板的 shadow root 里，可用户点「外面」时点的是主文档（比如左边聊天区），
  // 那一下根本进不了面板的监听器，菜单就一直挂着。所以在主文档再捕一个 pointerdown。
  // 用 composedPath 穿透 shadow 边界认领自己人：点「+」或菜单本身不算「外面」，
  // 否则会先关掉再打开，看着像点不动。
  function installGlobalMenuDismiss() {
    if (document.__starshipMenuDismiss) { return; }
    document.__starshipMenuDismiss = true;
    document.addEventListener(
      "pointerdown",
      function (event) {
        var path = typeof event.composedPath === "function" ? event.composedPath() : [];
        for (var index = 0; index < path.length; index += 1) {
          var node = path[index];
          if (!node || typeof node.closest !== "function") { continue; }
          if (node.closest(STARSHIP_MENU_ANCHOR_SELECTOR)) {
            return;
          }
        }
        var panels = document.querySelectorAll(PANEL_SELECTOR);
        for (var panelIndex = 0; panelIndex < panels.length; panelIndex += 1) {
          var shadow = panels[panelIndex].shadowRoot;
          if (shadow) { closeMenus(shadow); }
        }
      },
      true,
    );
  }
  // 官方面板类型行是**主文档**里的 light DOM（`openclaw-chat-sidebar-region`），
  // 而网页标签行在面板自己的 shadow root 里，两者不是同一棵树。要找「官方那个 +」
  // 只能从文档查：dashboard 会把访问过的 pane 全挂在 DOM 里，所以先按 pane 归属认领，
  // 认不到再退回文档里第一个带面板类型下拉的那一行。
  // 官方那个「+」自己拉的下拉就是星舰菜单的内容（星舰菜单读的正是它那 8 条，再补
  // 一条「标签页」），所以两个入口并存纯属重复。这里把官方「+」那一下拦下来，改开
  // 星舰菜单——按钮本体不动，屏幕上就只剩顶部这一个「+」。
  //
  // 监听挂在 document 而不是按钮上：官方面板是 Lit 渲染的，重渲染会造出全新的按钮
  // 节点，挂在旧节点上的监听器跟着一起没了。挂 document 只挂一次，靠
  // `event.composedPath()` 认人。pointerdown 和 click 都要拦——实测 `wa-dropdown`
  // 认的是 pointerdown，只拦 click 会先冒出官方那份下拉，和星舰菜单叠在一起。
  function panelForRailHeader(header) {
    var pane = header && typeof header.closest === "function"
      ? header.closest("openclaw-chat-pane")
      : null;
    var panels = (pane || document).querySelectorAll(PANEL_SELECTOR);
    var fallback = null;
    for (var index = 0; index < panels.length; index += 1) {
      var candidate = panels[index];
      var candidateRoot = panelRoot(candidate);
      if (!candidateRoot || !candidateRoot.querySelector(".bp-toolbar")) { continue; }
      if (!fallback) { fallback = candidate; }
      // dashboard 会把访问过的 pane 全留在 DOM 里，认屏幕上那个。
      if (
        typeof candidate.getClientRects === "function" &&
        candidate.getClientRects().length > 0
      ) {
        return candidate;
      }
    }
    return fallback;
  }
  function installHostAddMenu(panel) {
    if (document.__starshipHostAddMenu) { return; }
    document.__starshipHostAddMenu = true;
    function claim(event) {
      var path = typeof event.composedPath === "function" ? event.composedPath() : [];
      var trigger = null;
      for (var index = 0; index < path.length; index += 1) {
        var node = path[index];
        if (!node || typeof node.closest !== "function") { continue; }
        trigger = node.closest(HOST_ADD_TRIGGER_SELECTOR);
        if (trigger) { break; }
      }
      if (!trigger) { return; }
      var header = typeof trigger.closest === "function" ? trigger.closest(HOST_RAIL_SELECTOR) : null;
      var owner = panelForRailHeader(header || trigger) || panel || livePanelElement();
      var root = owner ? panelRoot(owner) : null;
      if (!root) { return; }
      var menu = root.querySelector(".starship-newtab__menu");
      if (!menu) {
        // 扫描还没轮到这块面板：就地装一次，别把官方那份下拉也一起挡掉。
        try {
          installParity(owner);
        } catch (error) {
          /* 装不上就退回官方行为。 */
        }
        menu = root.querySelector(".starship-newtab__menu");
      }
      if (!menu) { return; }
      event.preventDefault();
      event.stopImmediatePropagation();
      event.stopPropagation();
      if (event.type !== "click") { return; }
      if (menu.hidden) {
        openMenu(menu, trigger, root, newTabEntries(owner, root));
      } else {
        menu.hidden = true;
      }
    }
    document.addEventListener("pointerdown", claim, true);
    document.addEventListener("click", claim, true);
  }
  function hostRailHeaderFor(panel) {
    var headers = document.querySelectorAll(HOST_RAIL_SELECTOR);
    if (headers.length === 0) { return null; }
    var pane = panel && typeof panel.closest === "function"
      ? panel.closest("openclaw-chat-pane")
      : null;
    var candidate = null;
    for (var index = 0; index < headers.length; index += 1) {
      var header = headers[index];
      if (pane && pane.contains(header)) { return header; }
      if (candidate) { continue; }
      if (header.querySelector(OFFICIAL_ADD_DROPDOWN_SELECTOR)) {
        candidate = header;
      }
    }
    return candidate;
  }
  function installParity(panel) {
    var root = panelRoot(panel);
    if (!root) { return false; }
    var toolbar = root.querySelector(".bp-toolbar");
    if (!toolbar) { return false; }
    if (!root.querySelector("style[data-starship-parity]")) {
      var style = document.createElement("style");
      style.setAttribute("data-starship-parity", "codex");
      style.textContent = PARITY_CSS;
      root.appendChild(style);
    }
    adoptParitySheet(root);
    moveNewTabToRail(root, toolbar);
    installNewTabMenu(panel, root);
    installMenuDismiss(panel, root);
    installGlobalMenuDismiss();
    installHostAddMenu(panel);
    var toggle = root.querySelector(".starship-extras__toggle");
    if (!toggle) {
      toggle = document.createElement("button");
      toggle.className = "bp-icon starship-extras__toggle";
      toggle.type = "button";
      toggle.title = "\u661f\u8230\u6d4f\u89c8\u5668\u5de5\u5177";
      toggle.setAttribute("aria-label", "\u661f\u8230\u6d4f\u89c8\u5668\u5de5\u5177");
      toggle.setAttribute("aria-haspopup", "menu");
      // 工具菜单在工具行最右，右对齐到按钮才不会顶出面板边缘。
      toggle.setAttribute("data-starship-menu-right", "1");
      toggle.innerHTML = MENU_GLYPH;
      toggle.addEventListener("click", function (event) {
        event.preventDefault();
        event.stopPropagation();
        var menu = root.querySelector(".starship-extras__menu");
        if (!menu) { return; }
        if (menu.hidden) {
          openMenu(menu, toggle, root, extrasEntries(panel, root));
        } else {
          menu.hidden = true;
        }
      });
      toolbar.appendChild(toggle);
    }
    if (!root.querySelector(".starship-extras__menu")) {
      var menu = document.createElement("div");
      menu.className = "starship-extras__menu";
      menu.setAttribute("role", "menu");
      menu.hidden = true;
      fillMenu(menu, extrasEntries(panel, root));
      root.appendChild(menu);
    }
    if (!root.querySelector(".starship-newtab__menu")) {
      var newTabMenu = document.createElement("div");
      newTabMenu.className = "starship-newtab__menu";
      newTabMenu.setAttribute("role", "menu");
      newTabMenu.hidden = true;
      fillMenu(newTabMenu, newTabEntries(panel, root));
      root.appendChild(newTabMenu);
    }
    return true;
  }
  var parityPending = null;
  function scanPanels() {
    parityPending = null;
    if (!parityEnabled()) {
      return;
    }
    var panels = document.querySelectorAll(PANEL_SELECTOR);
    for (var index = 0; index < panels.length; index += 1) {
      try {
        installParity(panels[index]);
      } catch (error) {
        // The chrome layer must never take the official panel down with it.
      }
    }
  }
  function scheduleParityScan() {
    if (parityPending !== null) { return; }
    parityPending = window.setTimeout(scanPanels, 200);
  }
  document.addEventListener(
    "keydown",
    function (event) {
      if (!(event.ctrlKey || event.metaKey) || event.altKey) { return; }
      if (event.key !== "f" && event.key !== "F") { return; }
      var panel = livePanelElement();
      if (!panel) { return; }
      var root = panelRoot(panel);
      if (!root || !root.querySelector(".bp-toolbar")) { return; }
      event.preventDefault();
      openFindBar(panel, root);
    },
    true,
  );
  // 同上：document_start 时 documentElement 还不存在，直接 observe 会抛，
  // 一旦被 try/catch 吞掉，parity 层就只剩 2 秒轮询那条腿，页面刚起来时的
  // 面板扫描全部丢失。这里显式等到 documentElement 出现再挂。
  function watchPanelMutations() {
    var root = document.documentElement;
    if (!root) {
      window.setTimeout(watchPanelMutations, 25);
      return;
    }
    try {
      new MutationObserver(scheduleParityScan).observe(root, {
        childList: true,
        subtree: true,
        // 面板的「现在显示的是哪一个」是靠 class（pane 上的 --visible/--active）和
        // 标签上的 active 属性表达的，这两种变化都不产生 childList 记录。只看子节点
        // 的话，用户切到「审阅」之后要等最长 2 秒的轮询，那一行才回来；带上属性过滤
        // 后由 200ms 防抖接管，切换跟手，且 class 抖动不会把扫描放大（防抖封顶）。
        attributes: true,
        attributeFilter: ["class", "active", "hidden", "panel", "open"],
      });
    } catch (error) {
      /* MutationObserver is always available in WebView2. */
    }
  }
  watchPanelMutations();
  scanPanels();
  window.setInterval(scheduleParityScan, 2000);
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
            /// The dashboard has chrome open on top of the stage (the panel-type
            /// dropdown and friends). A native child view cannot be layered
            /// under HTML, so the shell has to hide the child view instead.
            occluded: bool,
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
        /// Dashboard chrome is open over the stage, so the native child view has
        /// to stay hidden or it would cover that chrome.
        occluded: bool,
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
        /// Last occlusion state reported by the probe, kept so the hide/show
        /// transition is logged once instead of on every heartbeat.
        occluded: bool,
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

    /// Wraps a WebView2 event closure so a panic in our body cannot abort the
    /// client.
    ///
    /// The runtime calls these closures on a stack it entered through
    /// `extern "system"`; a panic there cannot unwind, so Rust fast-fails the
    /// whole process (`0xc0000409`) with no log. Everything that runs inside the
    /// callback is therefore funnelled through `crash_log::guard`, which records
    /// the panic and returns a benign `Ok(())` instead.
    fn guarded_event<A, B, F>(
        label: &'static str,
        mut body: F,
    ) -> Box<dyn FnMut(A, B) -> windows::core::Result<()>>
    where
        F: FnMut(A, B) -> windows::core::Result<()> + 'static,
        A: 'static,
        B: 'static,
    {
        Box::new(move |arg_a, arg_b| {
            crate::crash_log::guard(label, || body(arg_a, arg_b)).unwrap_or(Ok(()))
        })
    }

    /// Same guard for the one-shot `*CompletedHandler` callbacks.
    fn guarded_completed<A, F>(
        label: &'static str,
        body: F,
    ) -> Box<dyn FnOnce(A, String) -> windows::core::Result<()>>
    where
        F: FnOnce(A, String) -> windows::core::Result<()> + 'static,
        A: 'static,
    {
        Box::new(move |arg_a, arg_b| {
            crate::crash_log::guard(label, || body(arg_a, arg_b)).unwrap_or(Ok(()))
        })
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
            let _ = crate::crash_log::guard("browser.with-webview.attach", move || {
                let core = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => core,
                    Err(error) => {
                        bridge_log(&format!(
                            "attach {attempt}: CoreWebView2 unavailable: {error}"
                        ));
                        schedule_attach_retry(&retry_app, retry_sender, attempt);
                        return;
                    }
                };
                let handler = WebMessageReceivedEventHandler::create(guarded_event(
                    "browser.dashboard-message",
                    move |_sender: Option<ICoreWebView2>,
                          args: Option<ICoreWebView2WebMessageReceivedEventArgs>| {
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
                            // The dashboard shim sends its first probe once it
                            // is injected, which is the earliest point where a
                            // restored tab session can be presented again.
                            if let Some(doc_id) = handshake {
                                let _ = handler_sender.send(Command::RestoreSession { doc_id });
                            }
                        }
                        Ok(())
                    },
                ));
                let mut token = 0i64;
                match unsafe { core.add_WebMessageReceived(&handler, &mut token) } {
                    Ok(_) => bridge_log(&format!(
                        "attach {attempt}: web message handler ready token={token}"
                    )),
                    Err(error) => {
                        bridge_log(&format!(
                            "attach {attempt}: add_WebMessageReceived failed: {error}"
                        ));
                        schedule_attach_retry(&retry_app, retry_sender, attempt);
                    }
                }
            });
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
            let occluded = probe
                .get("occluded")
                .and_then(Value::as_bool)
                .unwrap_or(false);
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
                occluded,
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
            occluded: false,
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
                    occluded,
                    ..
                } => {
                    self.probe = Some(ProbeSnapshot {
                        visible,
                        tab_id: tab_id.clone(),
                        scope: scope.clone(),
                        occluded,
                        at: Instant::now(),
                    });
                    let occlusion_changed = self.occluded != occluded;
                    if occlusion_changed {
                        bridge_log(if occluded {
                            "shell occluded: native browser view stepped aside"
                        } else {
                            "shell occlusion cleared: native browser view restored"
                        });
                        self.occluded = occluded;
                    }
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
                    if self.fallback != next || occlusion_changed {
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
                // Native child views always paint above the page, so while the
                // dashboard has a menu open over the stage the correct geometry
                // is "nowhere": showing the child view here is exactly what
                // hides the menu.
                if probe.occluded {
                    return winners;
                }
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
            let _ = crate::crash_log::guard("browser.with-webview.tab-events", move || {
                let Ok(core) = (unsafe { platform.controller().CoreWebView2() }) else {
                    return;
                };
                unsafe {
                let mut token = 0i64;

                let nav_sender = sender.clone();
                let nav_tab = tab_id.clone();
                let handler =
                    NavigationStartingEventHandler::create(guarded_event(
                        "browser.tab-navigation-starting",
                        move |_sender: Option<ICoreWebView2>,
                              args: Option<ICoreWebView2NavigationStartingEventArgs>| {
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
                    },
                    ));
                let _ = core.add_NavigationStarting(&handler, &mut token);

                let content_sender = sender.clone();
                let content_tab = tab_id.clone();
                let handler = ContentLoadingEventHandler::create(guarded_event(
                    "browser.tab-content-loading",
                    move |_sender: Option<ICoreWebView2>,
                          _args: Option<ICoreWebView2ContentLoadingEventArgs>| {
                        let _ = content_sender.send(Command::TabEvent {
                            tab_id: content_tab.clone(),
                            event: TabEvent::Loading(true),
                        });
                        Ok(())
                    },
                ));
                let _ = core.add_ContentLoading(&handler, &mut token);

                let source_sender = sender.clone();
                let source_tab = tab_id.clone();
                let handler = SourceChangedEventHandler::create(guarded_event(
                    "browser.tab-source-changed",
                    move |sender: Option<ICoreWebView2>, _args: Option<ICoreWebView2SourceChangedEventArgs>| {
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
                    },
                ));
                let _ = core.add_SourceChanged(&handler, &mut token);

                let completed_sender = sender.clone();
                let completed_tab = tab_id.clone();
                let handler =
                    NavigationCompletedEventHandler::create(guarded_event(
                        "browser.tab-navigation-completed",
                        move |sender: Option<ICoreWebView2>,
                              _args: Option<ICoreWebView2NavigationCompletedEventArgs>| {
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
                        },
                    ));
                let _ = core.add_NavigationCompleted(&handler, &mut token);

                let title_sender = sender.clone();
                let title_tab = tab_id.clone();
                let handler =
                    DocumentTitleChangedEventHandler::create(guarded_event(
                        "browser.tab-title-changed",
                        move |sender: Option<ICoreWebView2>, _args: Option<IUnknown>| {
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
                        },
                    ));
                let _ = core.add_DocumentTitleChanged(&handler, &mut token);

                let history_sender = sender.clone();
                let history_tab = tab_id.clone();
                let handler = HistoryChangedEventHandler::create(guarded_event(
                    "browser.tab-history-changed",
                    move |sender: Option<ICoreWebView2>, _args: Option<IUnknown>| {
                        if let Some(sender) = sender {
                            let _ = history_sender.send(Command::TabEvent {
                                tab_id: history_tab.clone(),
                                event: history_event(&sender),
                            });
                        }
                        Ok(())
                    },
                ));
                let _ = core.add_HistoryChanged(&handler, &mut token);

                // A crashed WebView2 process leaves an empty rectangle where
                // the panel should be. Recover in place instead of waiting for
                // the user to close and reopen the panel.
                let failed_sender = sender.clone();
                let failed_tab = tab_id.clone();
                let handler =
                    ProcessFailedEventHandler::create(guarded_event(
                        "browser.tab-process-failed",
                        move |_sender: Option<ICoreWebView2>,
                              args: Option<ICoreWebView2ProcessFailedEventArgs>| {
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
                        },
                    ));
                let _ = core.add_ProcessFailed(&handler, &mut token);

                let popup_sender = sender;
                let popup_tab = tab_id;
                let handler =
                    NewWindowRequestedEventHandler::create(guarded_event(
                        "browser.tab-new-window",
                        move |_sender: Option<ICoreWebView2>,
                              args: Option<ICoreWebView2NewWindowRequestedEventArgs>| {
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
                        },
                    ));
                let _ = core.add_NewWindowRequested(&handler, &mut token);
                }
            });
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
                let _ = crate::crash_log::guard("browser.with-webview.cdp", move || {
                    let core = match unsafe { platform.controller().CoreWebView2() } {
                        Ok(core) => core,
                        Err(_) => {
                            let _ = sender.send(None);
                            return;
                        }
                    };
                    let handler_sender = sender.clone();
                    let handler = CallDevToolsProtocolMethodCompletedHandler::create(
                        guarded_completed("browser.cdp-completed", move |_error, result| {
                            let _ = handler_sender.send(Some(result));
                            Ok(())
                        }),
                    );
                    if unsafe { core.CallDevToolsProtocolMethod(&method, &parameters, &handler) }
                        .is_err()
                    {
                        let _ = sender.send(None);
                    }
                });
            })
            .ok()?;
        receiver.recv_timeout(Duration::from_secs(10)).ok().flatten()
    }

    fn allowed_cdp_method(method: &str) -> bool {
        const PREFIXES: [&str; 5] = ["Input.", "Page.", "DOM.", "Runtime.", "Network."];
        // `Browser.*` is deliberately not a blanket prefix: only the download
        // behaviour surface the parity layer needs is reachable from a plugin.
        const EXACT: [&str; 1] = ["Browser.setDownloadBehavior"];
        method.len() < 64
            && method
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
            && (PREFIXES.iter().any(|prefix| method.starts_with(prefix))
                || EXACT.contains(&method))
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
            "zoom" => {
                const MIN_ZOOM: f64 = 0.25;
                const MAX_ZOOM: f64 = 3.0;
                const ZOOM_STEP: f64 = 0.1;
                let current = page_zoom(webview)
                    .ok_or_else(|| "Native browser tab is unavailable".to_string())?;
                let target = match message.get("direction").and_then(Value::as_str) {
                    Some("reset") => 1.0,
                    Some("in") => current + ZOOM_STEP,
                    Some("out") => current - ZOOM_STEP,
                    Some(other) => return Err(format!("Unsupported zoom direction: {other}")),
                    None => message
                        .get("factor")
                        .and_then(Value::as_f64)
                        .ok_or_else(|| "A zoom factor or direction is required".to_string())?,
                };
                if !target.is_finite() {
                    return Err("Invalid zoom factor".to_string());
                }
                let target = target.clamp(MIN_ZOOM, MAX_ZOOM);
                let applied = set_page_zoom(webview, target)
                    .ok_or_else(|| "Native browser zoom failed".to_string())?;
                Ok(json!({ "zoom": applied, "previous": current }))
            }
            "devtools" => {
                let mode = message
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("open");
                let result = match mode {
                    "open" => run_on_core(webview, |core| unsafe { core.OpenDevToolsWindow() }),
                    // The WebView2 SDK exposes `OpenDevToolsWindow` and nothing
                    // else: there is no `CloseDevToolsWindow` to call, so say so
                    // instead of pretending the window went away.
                    "close" => {
                        return Err(
                            "This WebView2 runtime cannot close DevTools programmatically".to_string(),
                        )
                    }
                    other => return Err(format!("Unsupported devtools mode: {other}")),
                };
                match result {
                    Some(Ok(())) => Ok(json!({ "mode": mode })),
                    Some(Err(error)) => Err(format!("DevTools failed: {error}")),
                    None => Err("Native browser tab is unavailable".to_string()),
                }
            }
            "find" => {
                let text = message
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.len() > 500 {
                    return Err("Search text is too long".to_string());
                }
                let params = json!({
                    "text": text,
                    "forward": message
                        .get("forward")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    "matchCase": message
                        .get("matchCase")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    "findNext": message
                        .get("findNext")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
                let raw = call_cdp(webview, "Page.findInPage", &cdp_params(params))
                    .ok_or_else(|| "CDP findInPage failed".to_string())?;
                let result = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
                Ok(json!({ "find": result }))
            }
            "findStop" => {
                let params = json!({ "action": "clearSelection" });
                call_cdp(webview, "Page.stopFindInPage", &cdp_params(params))
                    .ok_or_else(|| "CDP stopFindInPage failed".to_string())?;
                Ok(json!({}))
            }
            "downloads" => {
                let requested = message.get("path").and_then(Value::as_str);
                let directory = download_directory(requested)?;
                let behavior = message
                    .get("behavior")
                    .and_then(Value::as_str)
                    .unwrap_or("allow");
                if !matches!(behavior, "allow" | "deny" | "default") {
                    return Err("Unsupported download behaviour".to_string());
                }
                let display = directory.to_string_lossy().to_string();
                let params = json!({
                    "behavior": behavior,
                    "downloadPath": display,
                    "eventsEnabled": true,
                });
                call_cdp(webview, "Browser.setDownloadBehavior", &cdp_params(params))
                    .ok_or_else(|| "CDP setDownloadBehavior failed".to_string())?;
                let opened = message
                    .get("open")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if opened {
                    let _ = std::process::Command::new("explorer")
                        .arg(&directory)
                        .spawn();
                }
                Ok(json!({ "downloadPath": display, "behavior": behavior, "opened": opened }))
            }
            _ => Err(format!("Unsupported action: {action}")),
        }
    }

    fn execute_script(webview: &Webview, script: String) -> Option<String> {
        let (sender, receiver) = mpsc::channel();
        let javascript = HSTRING::from(script);
        webview
            .with_webview(move |platform| {
                let _ = crate::crash_log::guard("browser.with-webview.execute-script", move || {
                let core = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => core,
                    Err(_) => {
                        let _ = sender.send(None);
                        return;
                    }
                };
                let handler_sender = sender.clone();
                let handler =
                    ExecuteScriptCompletedHandler::create(guarded_completed(
                        "browser.execute-script-completed",
                        move |_error, result| {
                            let _ = handler_sender.send(Some(result));
                            Ok(())
                        },
                    ));
                if unsafe { core.ExecuteScript(&javascript, &handler) }.is_err() {
                    let _ = sender.send(None);
                }
                });
            })
            .ok()?;
        receiver.recv_timeout(Duration::from_secs(10)).ok().flatten()
    }

    fn capture_png(webview: &Webview) -> Option<String> {
        let (sender, receiver) = mpsc::channel();
        webview
            .with_webview(move |platform| {
                let _ = crate::crash_log::guard("browser.with-webview.capture-png", move || {
                let core = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => core,
                    Err(_) => {
                        let _ = sender.send(None);
                        return;
                    }
                };
                let handler_sender = sender.clone();
                let handler = CallDevToolsProtocolMethodCompletedHandler::create(
                    guarded_completed("browser.capture-screenshot-completed", move |_error, result| {
                        let data = serde_json::from_str::<Value>(&result)
                            .ok()
                            .and_then(|value| {
                                value.get("data").and_then(Value::as_str).map(str::to_string)
                            });
                        let _ = handler_sender.send(data);
                        Ok(())
                    }),
                );
                let method = HSTRING::from("Page.captureScreenshot");
                let parameters = HSTRING::from("{\"format\":\"png\"}");
                if unsafe { core.CallDevToolsProtocolMethod(&method, &parameters, &handler) }
                    .is_err()
                {
                    let _ = sender.send(None);
                }
                });
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
                let _ = crate::crash_log::guard("browser.with-webview.run-on-core", move || {
                let result = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => Some(action(&core)),
                    Err(_) => None,
                };
                let _ = sender.send(result);
                });
            })
            .ok()?;
        receiver.recv_timeout(Duration::from_secs(10)).ok().flatten()
    }

    /// Same handoff as [`run_on_core`], but for the controller: page zoom lives
    /// on `ICoreWebView2Controller`, not on `ICoreWebView2`.
    fn run_on_controller<T, F>(webview: &Webview, action: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce(&ICoreWebView2Controller) -> T + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        webview
            .with_webview(move |platform| {
                let _ = crate::crash_log::guard(
                    "browser.with-webview.run-on-controller",
                    move || {
                        let controller = platform.controller();
                        let _ = sender.send(Some(action(&controller)));
                    },
                );
            })
            .ok()?;
        receiver
            .recv_timeout(Duration::from_secs(10))
            .ok()
            .flatten()
    }

    /// Page zoom, clamped to the range Chromium itself accepts.
    fn page_zoom(webview: &Webview) -> Option<f64> {
        run_on_controller(webview, |controller| {
            let mut value = 0.0f64;
            unsafe { controller.ZoomFactor(&mut value) }.ok().map(|_| value)
        })
        .flatten()
    }

    fn set_page_zoom(webview: &Webview, factor: f64) -> Option<f64> {
        run_on_controller(webview, move |controller| {
            unsafe { controller.SetZoomFactor(factor) }.ok().map(|_| factor)
        })
        .flatten()
    }

    /// Downloads land in the user's Downloads folder unless the caller names a
    /// directory, so the shell never has to guess where a file went.
    fn download_directory(requested: Option<&str>) -> Result<std::path::PathBuf, String> {
        let path = match requested {
            Some(value) if !value.trim().is_empty() => std::path::PathBuf::from(value),
            _ => {
                let profile = std::env::var("USERPROFILE")
                    .map_err(|_| "USERPROFILE is not set".to_string())?;
                std::path::PathBuf::from(profile).join("Downloads")
            }
        };
        if path.as_os_str().len() > 260 {
            return Err("Download path is too long".to_string());
        }
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("Could not create the download folder: {error}"))?;
        Ok(path)
    }

    fn post_to_dashboard(app: &AppHandle, message: &Value) {
        let Ok(payload) = serde_json::to_string(message) else {
            return;
        };
        let Some(webview) = app.get_webview("main") else {
            return;
        };
        let _ = webview.with_webview(move |platform| {
            let _ = crate::crash_log::guard("browser.with-webview.post-dashboard", move || {
            let Ok(core) = (unsafe { platform.controller().CoreWebView2() }) else {
                return;
            };
            let payload = HSTRING::from(payload);
            let _ = unsafe { core.PostWebMessageAsJson(&payload) };
            });
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
