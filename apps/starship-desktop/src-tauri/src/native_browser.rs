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
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};
    use tauri::webview::{NewWindowResponse, WebviewBuilder};
    use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, Url, Webview, WebviewUrl};
    use webview2_com::{
        take_pwstr, BytesReceivedChangedEventHandler, CallDevToolsProtocolMethodCompletedHandler,
        ContentLoadingEventHandler, DocumentTitleChangedEventHandler, DownloadStartingEventHandler,
        ExecuteScriptCompletedHandler, FindStartCompletedHandler, HistoryChangedEventHandler,
        NavigationCompletedEventHandler,
        NavigationStartingEventHandler, NewWindowRequestedEventHandler,
        PermissionRequestedEventHandler, ProcessFailedEventHandler, ScriptDialogOpeningEventHandler,
        SourceChangedEventHandler, StateChangedEventHandler, WebMessageReceivedEventHandler,
    };
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2, ICoreWebView2Controller};
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2ContentLoadingEventArgs, ICoreWebView2Deferral,
        ICoreWebView2DownloadOperation, ICoreWebView2DownloadStartingEventArgs,
        ICoreWebView2NavigationCompletedEventArgs, ICoreWebView2NavigationStartingEventArgs,
        ICoreWebView2NewWindowRequestedEventArgs, ICoreWebView2PermissionRequestedEventArgs,
        ICoreWebView2ProcessFailedEventArgs, ICoreWebView2ScriptDialogOpeningEventArgs,
        ICoreWebView2SourceChangedEventArgs, ICoreWebView2WebMessageReceivedEventArgs,
        ICoreWebView2_4, COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON,
        COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED, COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED,
        COREWEBVIEW2_DOWNLOAD_STATE_IN_PROGRESS, COREWEBVIEW2_PERMISSION_KIND,
        COREWEBVIEW2_PERMISSION_STATE_ALLOW, COREWEBVIEW2_PERMISSION_STATE_DENY,
        COREWEBVIEW2_PREFERRED_COLOR_SCHEME_DARK, COREWEBVIEW2_PREFERRED_COLOR_SCHEME_LIGHT,
        COREWEBVIEW2_SCRIPT_DIALOG_KIND, ICoreWebView2Environment15, ICoreWebView2_13,
        ICoreWebView2_28,
    };
    use windows::core::{HSTRING, BOOL, PWSTR};
    use windows::core::Interface;
    use windows::core::IUnknown;

    /// Script evaluated in every native browser tab before the page's own
    /// scripts run.  This layer owns **page-internal behaviour**, not panel
    /// chrome, so it is deliberately kept out of `INIT_SCRIPT`.
    ///
    /// Why: 财联社这类新闻/门户站把几乎每个链接都写成
    /// `target="_blank" rel="noopener noreferrer"`（实测首页 291 个 `<a>` 里 268 个），
    /// 按浏览器原生语义，每次普通左键点击都会请求一个新窗口，落到面板上就是
    /// 「点一次多一个标签」：顺着列表读几条新闻，标签一路涨上去，焦点还被拽走。
    /// 这里把**普通左键点击**的 `_blank` 锚点改写为当前标签内导航——历史记录保留，
    /// 返回键照常可用。
    ///
    /// 刻意不动的部分：
    ///   * 脚本发起的 `window.open`：登录 / OAuth 这类必须新窗口的流程靠它，
    ///     改写会直接把它们弄坏；
    ///   * Ctrl/Cmd/Shift/Alt + 左键、中键：仍走原生新窗口路径（面板开新标签），
    ///     所以「我就是要开新标签」依旧可用；
    ///   * 带 `download` 属性的链接。
    ///
    /// 开发期可以用 `%LOCALAPPDATA%\ai.starship.client\dev\native-tab.js` 整段覆盖
    /// （见 `crate::native_browser_tab_script`），改策略不必重新编译。
    pub const TAB_INIT_SCRIPT: &str = r#"
(function () {
  if (window.__starshipTabLinkPolicy) { return; }
  window.__starshipTabLinkPolicy = true;
  document.addEventListener('click', function (event) {
    if (event.defaultPrevented || event.button !== 0) { return; }
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) { return; }
    var node = event.target;
    while (node && node.nodeType === 1 && node.tagName !== 'A') {
      node = node.parentNode;
    }
    if (!node || node.nodeType !== 1 || node.tagName !== 'A') { return; }
    if (node.hasAttribute('download')) { return; }
    if ((node.getAttribute('target') || '').toLowerCase() !== '_blank') { return; }
    var href = node.href || '';
    if (!href || href.indexOf('javascript:') === 0) { return; }
    event.preventDefault();
    window.location.href = href;
  }, true);

  // ── 智能体动作可视化 ───────────────────────────────────────────────────
  //
  // 壳层每做完一个动作，就把「这一下落在哪」投回来一次，这一层负责把它画出来：
  // 虚拟光标、按下环、动作标签、拖拽轨迹、滚动指示、元素描边。
  //
  // 为什么画在页面里而不是面板里：原生子 WebView2 是独立的 OS 子窗口，永远画在
  // dashboard 的 HTML 之上 —— 画在面板 shadow root 里的光标会被网页整块盖住。
  //
  // 为什么整层都是 `pointer-events:none`：它不参与命中测试。任何一处能吃到指针，
  // 页面自己的按钮就会「点不到」，而这类故障在截图上根本看不出来。
  //
  // 层是懒建的：没动作就一个节点都不加，页面零足迹。
  //
  // 外观取自 8.1 的 `CURSOR_MIRROR_SCRIPT` / `HOVER_HIGHLIGHT_SCRIPT`：22px 青圈
  // + 圈内 5px 白点 + 琥珀色按下环 + 动作小标签，悬停时另有一圈琥珀虚线框标出
  // 「指针下面那个元素」并写出它的名字。2.0.x 一度只剩一个 14px 蓝点加涟漪，
  // 看着单薄 —— 这里按旧版补回来，并保留现版新增的轨迹 / 滚动指示。
  var VISUAL_IDLE_MS = 6000;
  var PRESS_HOLD_MS = 240;
  var LABEL_HOLD_MS = 900;
  var visual = {
    layer: null,
    cursor: null,
    ring: null,
    label: null,
    outline: null,
    outlineLabel: null,
    idle: null,
    labelTimer: 0,
    pressTimer: 0,
    outlineTimer: 0,
  };
  function visualLayer() {
    if (visual.layer && visual.layer.isConnected) { return visual.layer; }
    var layer = document.createElement('div');
    layer.id = '__starshipVisualLayer';
    layer.setAttribute('data-starship-visual', '1');
    layer.setAttribute('aria-hidden', 'true');
    layer.style.cssText =
      'position:fixed;left:0;top:0;width:100%;height:100%;overflow:hidden;' +
      'pointer-events:none;z-index:2147483646;';
    // 挂在 documentElement 上而不是 body：有的站点整套重写 body，装饰层不该
    // 跟着被换掉；挂在 body 之外也就躲开了 body 自己的层叠上下文。
    (document.documentElement || document.body).appendChild(layer);
    visual.layer = layer;
    return layer;
  }
  function visualBox(css) {
    var node = document.createElement('div');
    node.setAttribute('data-starship-visual', '1');
    node.setAttribute('aria-hidden', 'true');
    node.style.cssText = css;
    return node;
  }
  function visualNumber(value) {
    var number = Number(value);
    return isFinite(number) ? number : null;
  }
  function visualPair(value) {
    if (!value || typeof value !== 'object') { return null; }
    var x = visualNumber(value.x);
    var y = visualNumber(value.y);
    if (x === null || y === null) { return null; }
    return { x: x, y: y };
  }
  function visualFadeOut(node, after) {
    var drop = function () {
      if (node.parentNode) { node.parentNode.removeChild(node); }
    };
    window.setTimeout(function () { node.style.opacity = '0'; }, Math.max(after - 220, 0));
    window.setTimeout(drop, after + 60);
  }
  function visualCursor() {
    if (visual.cursor && visual.cursor.isConnected) { return visual.cursor; }
    var cursor = visualBox(
      'position:fixed;left:0;top:0;width:22px;height:22px;margin:-11px 0 0 -11px;' +
      'border-radius:50%;border:2px solid rgba(0,229,255,0.95);' +
      'background:rgba(0,229,255,0.16);' +
      'box-shadow:0 0 14px rgba(0,229,255,0.6), inset 0 0 8px rgba(0,229,255,0.25);' +
      'pointer-events:none;opacity:0;will-change:transform;' +
      // 位移仍交给 transform（合成器），但时长/缓动保持旧版 90ms linear 的手感。
      'transition:transform 90ms linear, opacity 200ms ease;',
    );
    var ring = visualBox(
      'position:absolute;inset:-4px;border-radius:50%;' +
      'border:2px solid rgba(255,196,0,0.9);opacity:0;transform:scale(0.6);' +
      'transition:opacity 130ms ease, transform 160ms ease;',
    );
    var dot = visualBox(
      'position:absolute;left:50%;top:50%;width:5px;height:5px;margin:-2.5px 0 0 -2.5px;' +
      'border-radius:50%;background:#ffffff;box-shadow:0 0 6px rgba(255,255,255,0.9);',
    );
    var label = visualBox(
      'position:absolute;left:15px;top:11px;padding:1px 7px;border-radius:9px;' +
      'background:rgba(8,12,22,0.85);border:1px solid rgba(0,229,255,0.5);color:#7de9ff;' +
      "font:10px/15px 'Segoe UI','Microsoft YaHei',sans-serif;letter-spacing:0.4px;" +
      'white-space:nowrap;opacity:0;transform:translateY(2px);' +
      'transition:opacity 140ms ease, transform 140ms ease;',
    );
    cursor.appendChild(ring);
    cursor.appendChild(dot);
    cursor.appendChild(label);
    visualLayer().appendChild(cursor);
    visual.cursor = cursor;
    visual.ring = ring;
    visual.label = label;
    return cursor;
  }
  function visualPointTo(x, y) {
    if (x === null || y === null) { return; }
    var cursor = visualCursor();
    cursor.style.transform = 'translate3d(' + x + 'px,' + y + 'px,0)';
    cursor.style.opacity = '1';
    visualArm();
  }

  // 按下环：琥珀色 ring 从 scale(0.6) 张到 1.25，240ms 后收回（旧版时序）。
  function visualPress() {
    var cursor = visualCursor();
    cursor.style.opacity = '1';
    if (visual.ring) {
      visual.ring.style.opacity = '1';
      visual.ring.style.transform = 'scale(1.25)';
    }
    window.clearTimeout(visual.pressTimer);
    visual.pressTimer = window.setTimeout(function () {
      if (visual.ring) {
        visual.ring.style.opacity = '0';
        visual.ring.style.transform = 'scale(0.6)';
      }
    }, PRESS_HOLD_MS);
  }

  // 动作标签：写在光标右下角，默认 900ms 后淡出。
  function visualLabel(text, holdMs) {
    if (!text) { return; }
    visualCursor();
    if (!visual.label) { return; }
    visual.label.textContent = text;
    visual.label.style.opacity = '1';
    visual.label.style.transform = 'translateY(0)';
    window.clearTimeout(visual.labelTimer);
    visual.labelTimer = window.setTimeout(function () {
      if (visual.label) {
        visual.label.style.opacity = '0';
        visual.label.style.transform = 'translateY(2px)';
      }
    }, holdMs || LABEL_HOLD_MS);
    visualArm();
  }
  // 元素描边：琥珀虚线框，左上角跟一枚「元素名」小标签（旧版 HOVER_HIGHLIGHT）。
  function visualOutline(rect, text) {
    if (!rect) { return; }
    var width = Math.max(Math.min(rect.width, window.innerWidth), 2);
    var height = Math.max(Math.min(rect.height, window.innerHeight), 2);
    if (!visual.outline || !visual.outline.isConnected) {
      visual.outline = visualBox(
        'position:absolute;left:0;top:0;pointer-events:none;border-radius:3px;' +
        'border:2px dashed rgba(255,196,0,0.95);background:rgba(255,196,0,0.08);' +
        'opacity:0;' +
        'transition:transform 80ms linear,width 80ms linear,height 80ms linear,' +
        'opacity 200ms ease;',
      );
      visual.outlineLabel = visualBox(
        "position:absolute;left:0;top:-22px;padding:1px 7px;border-radius:8px;" +
        'background:rgba(8,12,22,0.9);border:1px solid rgba(255,196,0,0.55);' +
        "color:#ffd54d;font:10px/15px 'Segoe UI','Microsoft YaHei',sans-serif;" +
        'white-space:nowrap;',
      );
      visual.outline.appendChild(visual.outlineLabel);
      visualLayer().appendChild(visual.outline);
    }
    var outline = visual.outline;
    outline.style.width = width + 'px';
    outline.style.height = height + 'px';
    outline.style.transform = 'translate3d(' + rect.x + 'px,' + rect.y + 'px,0)';
    outline.style.opacity = '1';
    if (visual.outlineLabel) {
      visual.outlineLabel.textContent = text || '';
      visual.outlineLabel.style.display = text ? 'block' : 'none';
    }
    window.clearTimeout(visual.outlineTimer);
    visual.outlineTimer = window.setTimeout(function () {
      if (visual.outline) { visual.outline.style.opacity = '0'; }
    }, VISUAL_IDLE_MS);
    visualArm();
  }
  function visualRipple(x, y) {
    var ripple = visualBox(
      'position:absolute;left:0;top:0;width:22px;height:22px;margin:-11px 0 0 -11px;' +
      'border-radius:50%;border:2px solid rgba(0,229,255,0.75);' +
      'background:rgba(0,229,255,0.14);pointer-events:none;' +
      'transform:translate3d(' + x + 'px,' + y + 'px,0);',
    );
    visualLayer().appendChild(ripple);
    var drop = function () {
      if (ripple.parentNode) { ripple.parentNode.removeChild(ripple); }
    };
    if (typeof ripple.animate === 'function') {
      try {
        var animation = ripple.animate(
          [
            { transform: 'translate3d(' + x + 'px,' + y + 'px,0) scale(0.5)', opacity: 0.9 },
            { transform: 'translate3d(' + x + 'px,' + y + 'px,0) scale(3.2)', opacity: 0 },
          ],
          { duration: 520, easing: 'cubic-bezier(0.2,0.7,0.3,1)' },
        );
        animation.onfinish = drop;
        animation.oncancel = drop;
      } catch (error) { /* 动画不可用时下面那记兜底照样收尾 */ }
    }
    window.setTimeout(drop, 900);
  }
  function visualTrail(from, to) {
    var dx = to.x - from.x;
    var dy = to.y - from.y;
    var length = Math.sqrt(dx * dx + dy * dy);
    if (!isFinite(length) || length < 2) { return; }
    var angle = Math.atan2(dy, dx) * 180 / Math.PI;
    var line = visualBox(
      'position:absolute;left:0;top:0;height:3px;border-radius:2px;pointer-events:none;' +
      'transform-origin:0 50%;opacity:0.95;background:rgba(0,229,255,0.75);' +
      'width:' + length + 'px;' +
      'transform:translate3d(' + from.x + 'px,' + (from.y - 1.5) + 'px,0) ' +
      'rotate(' + angle + 'deg);',
    );
    visualLayer().appendChild(line);
    visualFadeOut(line, 1200);
  }
  function visualScroll(spec) {
    var deltaX = visualNumber(spec.deltaX) || 0;
    var deltaY = visualNumber(spec.deltaY) || 0;
    if (Math.abs(deltaX) < 0.5 && Math.abs(deltaY) < 0.5) { return; }
    var vertical = Math.abs(deltaY) >= Math.abs(deltaX);
    var toward = vertical ? deltaY : deltaX;
    var arrow = vertical
      ? (toward > 0 ? '\u2193' : '\u2191')
      : (toward > 0 ? '\u2192' : '\u2190');
    var pill = visualBox(
      'position:absolute;right:16px;top:50%;margin-top:-16px;padding:6px 10px;' +
      'border-radius:999px;pointer-events:none;opacity:0;' +
      "font:600 12px/1.2 'Segoe UI','Microsoft YaHei',sans-serif;" +
      'color:#c8f6ff;background:rgba(8,12,22,0.82);' +
      'box-shadow:0 0 0 1px rgba(0,229,255,0.45), 0 0 12px rgba(0,229,255,0.25);' +
      'transition:opacity 200ms ease;',
    );
    pill.textContent = arrow + ' ' + Math.round(Math.abs(toward)) + 'px';
    visualLayer().appendChild(pill);
    window.setTimeout(function () { pill.style.opacity = '1'; }, 0);
    visualFadeOut(pill, 1100);
  }
  function visualClear() {
    if (visual.cursor) { visual.cursor.style.opacity = '0'; }
    if (visual.label) { visual.label.style.opacity = '0'; }
    if (visual.ring) {
      visual.ring.style.opacity = '0';
      visual.ring.style.transform = 'scale(0.6)';
    }
    if (visual.outline) { visual.outline.style.opacity = '0'; }
  }
  function visualArm() {
    if (visual.idle !== null) { window.clearTimeout(visual.idle); }
    visual.idle = window.setTimeout(function () {
      visual.idle = null;
      visualClear();
    }, VISUAL_IDLE_MS);
  }
  // 动作 → 标签文案。文案逐字取自 8.1 的 `pulse(x, y, kind)`。
  var LABELS = {
    click: '\u2726 \u70b9\u51fb',
    select: '\u2726 \u70b9\u51fb',
    type: '\u270e \u8f93\u5165',
    press: '\u2328 \u6309\u952e',
    key: '\u2328 \u6309\u952e',
    hover: '\u2726 \u6307\u5411',
    drag: '\u21c4 \u62d6\u62fd',
  };
  // 悬停高亮只描可交互元素，否则满屏都在画框。
  var INTERACTIVE =
    'a,button,input,select,textarea,summary,[role="button"],[role="link"],' +
    '[role="tab"],[contenteditable="true"],[onclick]';
  function visualElementLabel(element) {
    if (!element) { return ''; }
    var text = (element.getAttribute('aria-label') || element.textContent || '')
      .replace(/\s+/g, ' ')
      .trim();
    var tag = element.tagName ? element.tagName.toLowerCase() : '';
    return tag + (text ? ' \u00b7 ' + text.slice(0, 36) : '');
  }
  // 一次动作的落点、描边、按下环、标签，集中在这里，免得散在 25 个动作分支里。
  function visualOnPage(action, x, y, rect, text, fromPoint, toPoint) {
    if (x !== null && y !== null) { visualPointTo(x, y); }
    if (action === 'drag') {
      var from = visualPair(fromPoint) || (x !== null ? { x: x, y: y } : null);
      var to = visualPair(toPoint);
      if (from) { visualPointTo(from.x, from.y); }
      if (from && to) { visualTrail(from, to); }
      if (to) { visualPointTo(to.x, to.y); }
      visualLabel(LABELS[action] || '');
      return;
    }
    if (rect) { visualOutline(rect, text); }
    if (action === 'click' || action === 'select') {
      if (x !== null && y !== null) { visualRipple(x, y); }
      visualPress();
    }
    visualLabel(LABELS[action] || '');
  }
  // 壳层唯一的入口。`spec` 由壳层拼好：`{action, ref, x, y, from, to, deltaX, deltaY}`。
  // 认不出的字段一律忽略，认不出的动作只画光标 —— 这一层永远不该让动作本身失败。
  function visualAct(spec) {
    if (!spec || typeof spec !== 'object') { return false; }
    var action = String(spec.action || '');
    var reference = typeof spec.ref === 'string' && spec.ref ? spec.ref : null;
    var element = null;
    if (reference) {
      try {
        element = document.querySelector('[data-starship-ref="' + reference + '"]');
      } catch (error) { element = null; }
    }
    var rect =
      element && typeof element.getBoundingClientRect === 'function'
        ? element.getBoundingClientRect()
        : null;
    var point = visualPair(spec);
    if (!point && rect) { point = { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 }; }
    if (action === 'scroll') {
      visualScroll(spec);
      if (point) { visualPointTo(point.x, point.y); }
      visualArm();
      return true;
    }
    visualOnPage(
      action,
      point ? point.x : null,
      point ? point.y : null,
      rect,
      element ? visualElementLabel(element) : '',
      spec.from,
      spec.to,
    );
    visualArm();
    return true;
  }
  // 给「不是走 act 进来的那一下」用：只画，不碰页面状态。
  // 壳层 `dispatch` 通道（原始 CDP Input.*）与手工演示都走这里。
  function visualPulse(x, y, kind) {
    visualPointTo(visualNumber(x), visualNumber(y));
    if (kind === 'click' || kind === 'select') { visualRipple(x, y); }
    visualPress();
    visualLabel(LABELS[kind] || '');
  }
  window.__starshipVisual = { act: visualAct, clear: visualClear, pulse: visualPulse };

  // 指针镜像的第二条驱动：壳层每做一次动作会先在页面里盖一个时间戳
  // `__starshipAgentInputUntil`（下面「人手优先」探针也用它）。在这个窗口里的指针
  // 事件就是智能体那一下 —— 不管它是壳层 act 送来的，还是从 `dispatch` 通道直接
  // 灌进来的 CDP Input.*。两条都画，就不会「有的动作看得见、有的看不见」。
  // 用户的真实鼠标不在这个窗口里，所以不会出现「你一动手，页面上还多一个假光标」。
  function agentInputActive() {
    return Date.now() < (window.__starshipAgentInputUntil || 0);
  }
  function onAgentPointerMove(event) {
    if (!agentInputActive()) { return; }
    if (typeof event.clientX !== 'number') { return; }
    visualPointTo(event.clientX, event.clientY);
    var element = null;
    try {
      element = event.target && event.target.closest ? event.target.closest(INTERACTIVE) : null;
    } catch (error) { element = null; }
    if (element && typeof element.getBoundingClientRect === 'function') {
      visualOutline(element.getBoundingClientRect(), visualElementLabel(element));
    }
  }
  function onAgentPointerDown(event) {
    if (!agentInputActive()) { return; }
    visualPress();
  }
  ['pointermove', 'mousemove', 'mouseover'].forEach(function (type) {
    document.addEventListener(type, onAgentPointerMove, { capture: true, passive: true });
  });
  ['pointerdown', 'mousedown'].forEach(function (type) {
    document.addEventListener(type, onAgentPointerDown, { capture: true, passive: true });
  });

  // -------------------------------------------------------------------------
  // 「人手优先」：用户和智能体共用一个页面，但不能抢同一块地方。
  //
  // 注意这不是「用户先做完，智能体才能动」——那样用户一边看，智能体就一边
  // 停着，等于没人干活。页面报的是**现场的坐标和目标**，由壳层按动作类型
  // 判断这一下到底会不会撞上：用户点左边、智能体点右边，互不相干，照常走。
  //
  // 判断只能从页面里取：子 WebView2 是挂在窗口上的独立 HWND，鼠标键盘在
  // WebView2 里就被吃掉了，宿主的消息循环一个字都看不到。`isTrusted` 是那条
  // 分界线 —— 真实输入为 true，网页脚本自己派发的合成事件为 false。
  //
  // 但壳层驱动动作走的 CDP `Input.*` 派出来的事件**也是** `isTrusted === true`，
  // 只看这个标记会把智能体认成用户。所以壳层在每次动作前后写一个毫秒时间戳
  // `window.__starshipAgentInputUntil`：窗口里的输入算智能体自己的，窗口一过
  // 人手照样被看见（两次动作之间通常隔着模型的思考时间，够长）。
  var humanSentAt = 0;
  // 事件命中的元素：扫描时打的 `data-starship-ref` 就在交互元素上，点到的
  // 往往是它里面的 `<span>`，所以往上找最近的带 ref 的祖先，壳层才有得比。
  function humanRef(node) {
    try {
      var el = node && node.closest ? node.closest('[data-starship-ref]') : null;
      return el ? el.getAttribute('data-starship-ref') : null;
    } catch (error) { return null; }
  }
  // 这一下是在往输入框里写字吗。写字只认键盘冲突（字会落进用户的光标位置），
  // 指针动作才看坐标 —— 敲键盘的时候鼠标停在哪儿跟这件事无关。
  function humanTyping(kind, node) {
    if (kind !== 'keydown') { return false; }
    try {
      var tag = node && node.tagName ? node.tagName.toLowerCase() : '';
      return tag === 'input' || tag === 'textarea' || (node && node.isContentEditable === true);
    } catch (error) { return false; }
  }
  function humanReport(kind, event) {
    // wheel 滚一下能来几十条事件，节流到 120ms 一条，壳层记的是「刚刚有人在动」。
    var now = Date.now();
    if (now - humanSentAt < 120) { return; }
    humanSentAt = now;
    // 人手一动，虚拟光标先撤：两个光标同时趴在页面上，看着就像在抢。
    try { visualClear(); } catch (error) { /* 可视化层没装上也不影响记账 */ }
    var node = event ? event.target : null;
    var payload = { __starshipTab: true, kind: kind };
    var reference = humanRef(node);
    if (reference) { payload.ref = reference; }
    if (event && typeof event.clientX === 'number') {
      // 键盘事件的 clientX 恒为 0，这里会自然跳过，不会伪造出一个 (0,0) 落点。
      if (event.clientX || event.clientY) {
        payload.x = Math.round(event.clientX);
        payload.y = Math.round(event.clientY);
      }
    }
    if (humanTyping(kind, node)) { payload.typing = true; }
    try {
      window.chrome.webview.postMessage(JSON.stringify(payload));
    } catch (error) { /* 桥不通时这条账丢了就算了 */ }
  }
  function humanSeen(event, kind) {
    if (!event || event.isTrusted !== true) { return; }
    if (Date.now() < (window.__starshipAgentInputUntil || 0)) { return; }
    humanReport(kind, event);
  }
  ['pointerdown', 'keydown', 'wheel', 'touchstart'].forEach(function (type) {
    window.addEventListener(
      type,
      function (event) { humanSeen(event, type); },
      { capture: true, passive: true },
    );
  });
})();
"#;

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
      return;
    }
    if (data.__starshipStandIn === true) {
      try {
        applyStandIn(data);
      } catch (error) {
        /* 面板还没上屏时贴不上；下一帧由 syncStandIn 补。 */
      }
    }
  });
  var host = window.webkit || (window.webkit = {});
  var handlers = host.messageHandlers || (host.messageHandlers = {});
  handlers.openclawBrowser = { postMessage: postMessage };
  if (!window.__OPENCLAW_NATIVE_BROWSER__) {
    window.__OPENCLAW_NATIVE_BROWSER__ = { revision: 0, tabs: [] };
  }
  // ── 主题上报 ────────────────────────────────────────────────────────────
  // 内嵌网页的 `prefers-color-scheme` 由 WebView2 的颜色方案决定，默认跟着
  // Windows，而不是跟着星舰自己的主题。这里把官方 UI 当前的主题（它写在
  // `<html data-theme>` / `wa-light|wa-dark` 类上）报给壳层，壳层再把它设到
  // 标签 webview 的 profile 上，面板里的网页才会跟星舰同色。
  // 只在真的变了时才发一条，不参与请求-应答，也不需要回包。
  var lastReportedTheme = "";
  function reportTheme() {
    var root = document.documentElement;
    if (!root) { return; }
    var theme = root.getAttribute("data-theme") || "";
    if (theme !== "light" && theme !== "dark") {
      theme = root.classList.contains("wa-dark") ? "dark" : "light";
    }
    if (theme === lastReportedTheme) { return; }
    lastReportedTheme = theme;
    try {
      window.chrome.webview.postMessage(JSON.stringify({ __starshipTheme: theme }));
    } catch (error) {
      /* 桥不通时这条信号丢了就算了，网页按系统配色渲染 */
    }
  }
  // 注入脚本在 document 刚建立时就跑，`document.documentElement` 这时可能还不存在
  // （本文件其它注入层同样要 `mount()` 重试）。首条消息也可能早于壳层把消息处理器
  // 挂上去，所以除了挂观察者，还在起步阶段补报几次 —— 主题是幂等信号，多报无副作用。
  function mountThemeWatch() {
    var root = document.documentElement;
    if (!root) {
      window.requestAnimationFrame(mountThemeWatch);
      return;
    }
    reportTheme();
    try {
      new MutationObserver(reportTheme).observe(root, {
        attributes: true,
        attributeFilter: ["data-theme", "class"],
      });
    } catch (error) {
      /* 老 runtime 没有 MutationObserver 时只上报一次初始主题 */
    }
  }
  mountThemeWatch();
  [400, 1500, 4000].forEach(function (delay) {
    window.setTimeout(reportTheme, delay);
  });
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
        best = {
          active: isActive,
          tabId: tabId,
          scope: scope,
          stage: stage,
          rect: rect,
          // 星舰把面板自己那条标签行抬进官方一行之后，`.bp-stage` 才上移到合并后的
          // 位置。壳层要靠这一位分辨「这份几何是合并后的真值」还是「官方重挂面板时
          // 量到的、标签行还在第二行的中间态」——后者不能拿来做兜底基准。
          merged: panel.getAttribute(RAIL_MERGE_ATTR) === "1",
        };
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
  // 动作可视化层（虚拟光标/涟漪/拖拽轨迹/滚动指示/元素描边）是
  // `pointer-events:none` 的纯装饰。它压着网页不等于「原生视图该让位」——
  // 把它算成遮挡就会走回那只老妖怪：壳层把子 WebView2 换成同位置的截图、
  // 再换回来，闪白与抖动都是这么来的。所以先立白名单，再谈探测。
  // 页面侧那一层（`TAB_INIT_SCRIPT`）也带同一个属性；今后面板侧若加同一层
  // HUD，这里一并豁免，免得两条路各写一份判断。
  var VISUAL_MARKER = "data-starship-visual";
  function isVisualDecoration(node) {
    if (!node || typeof node.getAttribute !== "function") { return false; }
    if (node.getAttribute(VISUAL_MARKER)) { return true; }
    var parent = node.parentElement;
    return !!(parent && parent.closest && parent.closest("[" + VISUAL_MARKER + "]"));
  }
  function visibleOverlayRects() {
    var rects = [];
    for (var s = 0; s < OVERLAY_SELECTORS.length; s += 1) {
      var nodes = document.querySelectorAll(OVERLAY_SELECTORS[s]);
      for (var n = 0; n < nodes.length; n += 1) {
        var node = nodes[n];
        if (node.hidden) { continue; }
        if (isVisualDecoration(node)) { continue; }
        // The host matters too: shell-owned menus are plain elements whose own
        // rect is the whole popup.
        overlaySurfaceRects(node, rects);
        var shadow = node.shadowRoot;
        if (!shadow) { continue; }
        var surfaces = shadow.querySelectorAll(OVERLAY_SURFACE_SELECTOR);
        for (var i = 0; i < surfaces.length; i += 1) {
          if (isVisualDecoration(surfaces[i])) { continue; }
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
      var menus = root.querySelectorAll(STARSHIP_PROBE_OVERLAY_SELECTOR);
      for (var item = 0; item < menus.length; item += 1) {
        if (menus[item].hidden) { continue; }
        if (isVisualDecoration(menus[item])) { continue; }
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
  // 遮挡期间的「原生视图替身」。
  //
  // 原生子 WebView2 是独立的 OS 子窗口，永远画在网页之上，所以菜单一开，壳层只能
  // 把子视图藏起来——但「藏起来」到「露出空白」之间差一步：壳层在 hide 之前先把
  // 当前画面截成一张图（`__starshipStandIn`）投回来，这里把它贴回面板原位。于是
  // 用户看到的是「菜单浮在网页上」，而不是点一下工具就整块变白。
  // 图必须挂在 `.bp-stage` 里：菜单是面板 shadow root 上的 z-index 40 浮层，替身
  // 用更低的层级，菜单才不会被自己人盖住；`pointer-events:none` 让面板照旧收得到
  // 点击（点空白处照样关菜单）。
  var STARSHIP_STANDIN_CLASS = "starship-standin";
  var STARSHIP_STANDIN_STYLE =
    "position:absolute;left:0;top:0;width:100%;height:100%;" +
    // 兜底底色跟着主题走：浅色主题下不该在面板里留一块深色。
    "object-fit:fill;z-index:30;pointer-events:none;background:var(--bg, #0e1015);";
  var standinTabId = null;
  var standinSrc = "";
  var standinImage = null;
  // 只有「当前正在显示的那个标签」才配得上这张图：面板换标签之后旧替身必须撤掉，
  // 否则会拿上一页的画面盖住新页面。
  function standinStage() {
    var measurement = livePanelMeasurement();
    if (!measurement) { return null; }
    if (standinTabId && measurement.tabId !== standinTabId) { return null; }
    return measurement.stage;
  }
  function dropStandIn() {
    if (standinImage && standinImage.parentNode) {
      standinImage.parentNode.removeChild(standinImage);
    }
    standinImage = null;
  }
  function placeStandIn(stage) {
    if (standinImage && standinImage.parentNode !== stage) { dropStandIn(); }
    if (!standinImage) {
      standinImage = document.createElement("img");
      standinImage.className = STARSHIP_STANDIN_CLASS;
      standinImage.setAttribute("style", STARSHIP_STANDIN_STYLE);
      standinImage.setAttribute("aria-hidden", "true");
      standinImage.alt = "";
      standinImage.src = standinSrc;
      stage.appendChild(standinImage);
      return;
    }
    if (standinImage.getAttribute("src") !== standinSrc) {
      standinImage.setAttribute("src", standinSrc);
    }
  }
  function applyStandIn(message) {
    if (!message.visible) {
      standinTabId = null;
      standinSrc = "";
      dropStandIn();
      return;
    }
    if (typeof message.image !== "string" || !message.image) { return; }
    standinTabId = typeof message.tabId === "string" ? message.tabId : null;
    standinSrc = message.image;
    var stage = standinStage();
    if (stage) { placeStandIn(stage); }
  }
  // 官方重挂面板时 `.bp-stage` 会被整块换掉，替身跟着一起消失；只要菜单还开着，
  // 每 250ms 的探针轮询就顺手把它贴回去（没有遮挡时这个函数立即返回）。
  function syncStandIn() {
    if (!standinSrc) { return; }
    var stage = standinStage();
    if (!stage) { dropStandIn(); return; }
    if (standinImage && standinImage.parentNode === stage) { return; }
    placeStandIn(stage);
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
        merged: Boolean(measurement.merged),
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
    syncStandIn();
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
/* ---------------------------------------------------------------------------
   两行并一行（见 liftTabRowToRail）。
   官方右侧栏第一行是「面板类型胶囊 + 「+」…… 最右关闭」，面板自己那条网页标签行
   单占第二行。同一块面板上因此并排站着两条横栏，用户读到的就是「重复」——正是
   「官方UI和我们内置的还在重复」那句话。
   这里把面板内部那条标签行整体抬进官方那一行：官方控件留在原处不动，标签行缩进
   官方「+」和最右关闭按钮之间那段空白，第二条横栏整条消失，工具行随之上移一条
   横栏的高度，浏览区多回 48px。
   抬升的前提是「量到了官方那一行」，量到才写 :host([data-starship-rail-merged])。
   官方改名、面板被挪出侧栏、或者量出来的几何不合法时属性不写，整组规则自动失效，
   界面退回官方原本的两行形态——宁可不好看，也不出现半抬不抬的错位。 */
:host([data-starship-rail-merged]) :is(.bp--embedded, .bp--right, .bp--bottom) .bp-header {
  position: absolute;
  top: calc(-1 * var(--starship-rail-height, 48px));
  left: var(--starship-rail-inset, 0px);
  right: var(--starship-rail-outset, 0px);
  z-index: 4;
  height: var(--starship-rail-height, 48px);
  min-height: var(--starship-rail-height, 48px);
  padding: 0;
  align-items: center;
  background: transparent;
  /* 官方那一行自带下边框；这里再画一条会在同一像素上叠出深浅不一的两段。 */
  border-bottom: 0;
}
:host([data-starship-rail-merged]) .bp--embedded {
  /* 绝对定位的标签行要探到面板盒子外面（面板是从 grid 第二行 y=48 起算的）。 */
  overflow: visible;
}
:host([data-starship-rail-merged]) :is(.bp--embedded, .bp--right, .bp--bottom) .bp-header .tabstrip {
  height: 28px;
  align-items: center;
}
:host([data-starship-rail-merged]) :is(.bp--embedded, .bp--right, .bp--bottom) .bp-header .tabstrip .tabstrip-tab {
  /* 官方那一行的控件统一 28px 高、y=10 起；标签跟着这个尺码才和左边的胶囊齐平。 */
  height: 28px;
  align-self: center;
  border-radius: 14px;
}
:host([data-starship-rail-merged]) :is(.bp--embedded, .bp--right, .bp--bottom) .bp-header .tabstrip-tab__close {
  align-self: center;
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
/* 地址栏历史下拉。官方地址栏只是一个「填 URL 按回车」的框，没有记忆；这里补上
   Codex 那一列「favicon + 标题 + 域名」。定位用 fixed：面板容器自己有 overflow
   裁剪，absolute 会被切掉半截，坐标由 JS 按 \`.bp-url\` 的实时 rect 算。 */
.starship-addr__menu {
  position: fixed;
  z-index: 60;
  max-height: 342px;
  overflow-y: auto;
  padding: 4px;
  border: 1px solid var(--border, #262b34);
  border-radius: 10px;
  background: var(--panel, #14171e);
  box-shadow: 0 14px 34px rgba(0, 0, 0, 0.45);
  font-size: 12.5px;
}
.starship-addr__menu[hidden] { display: none; }
.starship-addr__row {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 6px 8px;
  border: 0;
  border-radius: 7px;
  background: transparent;
  color: var(--text, #d7dae0);
  font: inherit;
  text-align: left;
  cursor: default;
}
.starship-addr__row[data-active="1"] { background: var(--hover, #1e232c); }
.starship-addr__icon {
  flex: none;
  width: 16px;
  height: 16px;
  border-radius: 4px;
  object-fit: contain;
}
/* 抓不到图标的站点退化成首字母色块：色相由域名哈希定，同一站永远同色。 */
.starship-addr__glyph {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 16px;
  height: 16px;
  border-radius: 4px;
  color: #fff;
  font-size: 9.5px;
  font-weight: 700;
}
.starship-addr__title {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.starship-addr__host {
  flex: 0 1 auto;
  max-width: 44%;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  color: var(--muted, #8a919e);
  font-size: 11.5px;
}
.starship-addr__empty {
  padding: 10px 8px;
  color: var(--muted, #8a919e);
  font-size: 11.5px;
  text-align: center;
}
/* 底部那条「当前页」：Codex 的地址栏下拉最底下也有一条只有域名的灰行。 */
.starship-addr__foot {
  margin: 4px 2px 0;
  padding: 6px 8px 2px;
  border-top: 1px solid var(--border, #262b34);
  color: var(--muted, #8a919e);
  font-size: 11px;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
/* 下载面板。Codex 的下载是工具栏上一颗常驻按钮 + 一张能看进度、能打开文件的
   列表；官方面板里连「文件下到哪了」都没有。星舰把它挂进 ⋮ 工具菜单（官方
   那一条工具行已被标签行占满，再钉一颗常驻按钮会挤掉用户自己的控件），清单
   本身照 Codex 的形态做：文件名 + 来源域名 + 进度/结果，点一行打开那个文件。
   定位用 fixed：面板容器自己有 overflow 裁剪，absolute 会被切掉半截。 */
.starship-dl__menu {
  position: fixed;
  z-index: 60;
  max-height: 60vh;
  overflow-y: auto;
  padding: 6px;
  border: 1px solid var(--border, #262b34);
  border-radius: 10px;
  background: var(--panel, #14171e);
  box-shadow: 0 14px 34px rgba(0, 0, 0, 0.45);
  font-size: 12.5px;
}
.starship-dl__menu[hidden] { display: none; }
.starship-dl__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 2px 4px 6px;
  color: var(--text, #d7dae0);
  font-size: 12px;
  font-weight: 600;
}
.starship-dl__actions { display: flex; gap: 6px; }
.starship-dl__action {
  padding: 3px 8px;
  border: 1px solid var(--border, #262b34);
  border-radius: 7px;
  background: transparent;
  color: var(--muted, #8a919e);
  font: inherit;
  font-size: 11.5px;
  cursor: default;
}
.starship-dl__action:hover,
.starship-dl__action:focus-visible {
  background: var(--hover, #1e232c);
  color: var(--text, #d7dae0);
}
.starship-dl__row {
  display: block;
  width: 100%;
  padding: 7px 8px;
  border: 0;
  border-radius: 7px;
  background: transparent;
  color: var(--text, #d7dae0);
  font: inherit;
  text-align: left;
  cursor: default;
}
.starship-dl__row[data-active="1"] { background: var(--hover, #1e232c); }
.starship-dl__name {
  display: block;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.starship-dl__meta {
  display: block;
  margin-top: 2px;
  color: var(--muted, #8a919e);
  font-size: 11px;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.starship-dl__bar {
  display: block;
  height: 3px;
  margin-top: 6px;
  border-radius: 2px;
  background: rgba(255, 255, 255, 0.12);
  overflow: hidden;
}
.starship-dl__bar > i {
  display: block;
  height: 100%;
  border-radius: 2px;
  background: #4c8dff;
}
/* 拿不到 Content-Length 的下载（total 为负）画不了百分比，用一条来回跑的
   短条表示「还在下」，别假装 0%。 这一段在 JS 模板字符串里，注释里也不能出现
   反引号：一个未转义的反引号会提前闭合模板，整段注入脚本变成语法错误 ——
   症状不是样式坏掉，而是星舰浏览器层整个不装（面板退回官方原样）。 */
.starship-dl__bar[data-unknown="1"] > i {
  width: 34%;
  animation: starship-dl-slide 1.2s ease-in-out infinite alternate;
}
@keyframes starship-dl-slide {
  from { margin-left: 0; }
  to { margin-left: 66%; }
}
.starship-dl__empty {
  padding: 12px 8px;
  color: var(--muted, #8a919e);
  font-size: 11.5px;
  text-align: center;
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
  // 官方那一行右边的动作区（最右那个关闭按钮就在里面）。合并时要把它整块让出来，
  // 否则抬上来的标签行会盖住关闭按钮。
  var HOST_RAIL_ACTIONS_SELECTOR = ".side-panel__actions";
  // 抬到官方一行之后，官方面板容器 `overflow:hidden` 会把探出去的那一条裁掉，所以在
  // 主文档里放开这一个容器的裁剪。类名只在合并成功时挂上，其它面板类型不受影响。
  var RAIL_MERGE_CLASS = "starship-rail-merged";
  var RAIL_MERGE_ATTR = "data-starship-rail-merged";
  // 官方在「切面板类型 / 切标签 / pane 在 cache 之间搬家」时会先插节点再补几何，
  // 那一两帧里面板量出来是 0×0。早先这里一量不到就撤合并，用户看到的就是
  // 「点一下 → 标签行掉回第二行 → 过一会儿才并回一行」。改成连续几次都放不下
  // 才撤，撤销前保持已有形态。
  var RAIL_MERGE_DROP_STRIKES = 3;
  var RAIL_MERGE_CSS =
    ".side-panel__panel." + RAIL_MERGE_CLASS + " { overflow: visible !important; }";
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
  // 地址栏历史下拉是第三个浮层，但它有自己的开关逻辑（焦点、方向键、Esc），不跟
  // 上面那两个共用「点外面关掉」的手势，所以单独列一份只给遮挡探测用的名单。少列
  // 一个浮层的后果不是菜单关不掉，而是壳层不知道有东西盖在网页上、不让原生视图，
  // 下拉的下半截直接被网页画掉。
  var STARSHIP_PROBE_OVERLAY_SELECTOR =
    STARSHIP_MENU_SELECTOR + ", .starship-addr__menu, .starship-dl__menu";
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
        if (!detail) { return; }
        // 原生 Find（`engine:WebView2Find`）能给出总数与当前序号；老 runtime 退回
        // `window.find()` 时只有「找到 / 没找到」，如实显示 ✔ / ✖，不假装有计数。
        if (detail.counted === false) {
          count.textContent = detail.found ? "\u2714" : "\u2716";
        } else if (typeof detail.matches === "number") {
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
      label: "\u9002\u914d\u5bbd\u5ea6",
      hint: "\u81ea\u52a8",
      run: function () { actOnPanel(panel, "zoom", { direction: "fit" }); },
    });
    entries.push({
      group: "\u5de5\u5177",
      label: "\u5f00\u53d1\u8005\u5de5\u5177",
      hint: "F12",
      run: function () { actOnPanel(panel, "devtools", { mode: "open" }); },
    });
    entries.push({
      group: "\u5de5\u5177",
      label: "\u4e0b\u8f7d",
      hint: "Ctrl+J",
      run: function () { openDownloadsMenu(panel, root); },
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
  // 放开官方面板容器的裁剪。样式表要落一次在主文档里（PARITY_CSS 只进面板的
  // shadow root，管不到外面那一层），之后靠类名开关，不反复插节点。
  function ensureRailMergeStyle() {
    if (document.__starshipRailMergeStyle) { return; }
    document.__starshipRailMergeStyle = true;
    try {
      var style = document.createElement("style");
      style.setAttribute("data-starship-rail-merge", "1");
      style.textContent = RAIL_MERGE_CSS;
      (document.head || document.documentElement).appendChild(style);
    } catch (error) {
      /* 插不进去就退回两行形态，面板本身不受影响。 */
    }
  }
  // 把面板自己那条网页标签行抬进官方那一行（见 PARITY_CSS 里那段注释）。
  // 左右两端让给官方控件：左边让官方「+」，右边让最右那个关闭按钮，中间那段空白
  // 就是标签行的容身之处。两个值都在这里实测出来写成自定义属性，官方换宽度、
  // 面板类型胶囊变长都跟着重算——重算挂在既有的扫描循环上，不额外加计时器。
  function liftTabRowToRail(panel, root) {
    var header = root.querySelector(".bp-header");
    var rail = header ? hostRailHeaderFor(panel) : null;
    var trigger = rail ? rail.querySelector(HOST_ADD_TRIGGER_SELECTOR) : null;
    var actions = rail ? rail.querySelector(HOST_RAIL_ACTIONS_SELECTOR) : null;
    if (!header || !rail || !trigger) {
      // 结构性失效：官方那一行本身没了（面板被挪出侧栏 / 上游改名）。这类变化
      // 不会自己恢复，立刻撤合并，绝不半抬半不抬。
      panel.__starshipRailStrikes = 0;
      dropRailMerge(panel);
      return false;
    }
    var panelRect = panel.getBoundingClientRect();
    var railRect = rail.getBoundingClientRect();
    var triggerRect = trigger.getBoundingClientRect();
    var actionsRect = (actions || trigger).getBoundingClientRect();
    var inset = Math.round(triggerRect.right - panelRect.left + 6);
    var outset = Math.round(panelRect.right - actionsRect.left + 8);
    var measurable =
      panelRect.width && railRect.height && triggerRect.width && actionsRect.width;
    var fits =
      measurable &&
      inset >= 8 && outset >= 8 &&
      panelRect.width - inset - outset >= 120;
    if (!fits) {
      // 量不出来 = 面板正在上屏（dashboard 把访问过的 pane 都留在 DOM 里）；
      // 量得出来但放不下 = 官方那一行太窄（窗口被拖到极窄）。两者都可能只是一
      // 帧的抖动，所以都要连着撞墙几次才真的退回两行；在那之前保留已有形态，
      // 用户看不到「点一下就掉一行」的闪烁。
      var strikes = (panel.__starshipRailStrikes || 0) + 1;
      panel.__starshipRailStrikes = strikes;
      if (strikes >= RAIL_MERGE_DROP_STRIKES) {
        dropRailMerge(panel);
      }
      return false;
    }
    panel.__starshipRailStrikes = 0;
    ensureRailMergeStyle();
    panel.style.setProperty("--starship-rail-inset", inset + "px");
    panel.style.setProperty("--starship-rail-outset", outset + "px");
    panel.style.setProperty("--starship-rail-height", Math.round(railRect.height) + "px");
    var holder = typeof panel.closest === "function"
      ? panel.closest(".side-panel__panel")
      : null;
    if (holder) { holder.classList.add(RAIL_MERGE_CLASS); }
    var fresh = panel.getAttribute(RAIL_MERGE_ATTR) !== "1";
    panel.setAttribute(RAIL_MERGE_ATTR, "1");
    if (fresh) {
      // 刚写上合并属性：`.bp-stage` 立刻上移一条横栏的高度。壳层必须马上拿到
      // 新几何，不能等 250ms 的探针轮询——那段空窗里壳层会退回官方 present
      // 的旧几何（第二行的 y），用户看到的就是「几秒才回到第一行」。
      publishShellProbe(true);
    }
    return true;
  }
  function dropRailMerge(panel) {
    if (!panel.hasAttribute(RAIL_MERGE_ATTR)) { return; }
    panel.removeAttribute(RAIL_MERGE_ATTR);
    panel.__starshipRailStrikes = 0;
    var holder = typeof panel.closest === "function"
      ? panel.closest(".side-panel__panel")
      : null;
    if (holder) { holder.classList.remove(RAIL_MERGE_CLASS); }
  }
  // 面板内部的结构（`.bp-header` / `.bp-stage` / 标签行 / 工具行）长在面板自己的
  // shadow root 里，document 级 observer 一条记录都收不到。给每块面板单独挂一个
  // 观测器，只在这块面板**还没有合并形态**时催一次下一帧重扫：`installParity` 那次
  // 因为内容没渲染完而空转的调用，会在官方把内容插进来的一帧内被补上，标签行不用
  // 等慢扫描。合并完成之后观测器不再参与，地址栏 / 标题 / 进度条这些高频抖动不会
  // 触发重算。
  var ROOT_MUTATION_SELECTOR =
    ".bp-header, .bp-stage, .bp-toolbar, .tabstrip, wa-tab";
  function rootMutationMatters(records) {
    for (var index = 0; index < records.length; index += 1) {
      var record = records[index];
      if (record.type === "attributes") {
        var target = record.target;
        if (
          target &&
          target.nodeType === 1 &&
          typeof target.matches === "function" &&
          target.matches(ROOT_MUTATION_SELECTOR)
        ) {
          return true;
        }
      }
      var added = record.addedNodes;
      for (var node = 0; added && node < added.length; node += 1) {
        var element = added[node];
        if (!element || element.nodeType !== 1) { continue; }
        if (typeof element.matches !== "function") { continue; }
        if (
          element.matches(ROOT_MUTATION_SELECTOR) ||
          element.querySelector(ROOT_MUTATION_SELECTOR)
        ) {
          return true;
        }
      }
    }
    return false;
  }
  function watchPanelRoot(panel, root) {
    if (panel.__starshipRootObserver) { return; }
    if (typeof MutationObserver !== "function") { return; }
    try {
      panel.__starshipRootObserver = new MutationObserver(function (records) {
        // 已经合并过的面板不需要抢帧；撞墙退回两行（属性被撤）之后会重新参与。
        if (panel.getAttribute(RAIL_MERGE_ATTR) === "1") { return; }
        if (!rootMutationMatters(records)) { return; }
        scheduleParityScan(true);
      });
      panel.__starshipRootObserver.observe(root, {
        childList: true,
        subtree: true,
        // class 决定 `.bp-stage` 是不是已经交给了内嵌视图；hidden / active 决定
        // 标签行和工具行有没有出现。三样都会改变能不能合并，其余属性不管。
        attributes: true,
        attributeFilter: ["class", "active", "hidden"],
      });
    } catch (error) {
      panel.__starshipRootObserver = null;
    }
  }
  function installParity(panel) {
    var root = panelRoot(panel);
    if (!root) { return false; }
    // 先挂观测器再找工具行：官方重挂面板时先插一个空壳，里面的 `.bp-toolbar` /
    // `.bp-header` 要下一帧才渲染出来。这一刻直接返回 false 之后就没人再盯着这块
    // shadow root ——document 级 observer 穿不透 shadow 边界，合并只能等 200ms
    // 防抖甚至 2 秒轮询才补上，而这几百毫秒里探针量到的是**还没合并**的几何。
    watchPanelRoot(panel, root);
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
    liftTabRowToRail(panel, root);
    installNewTabMenu(panel, root);
    installMenuDismiss(panel, root);
    installGlobalMenuDismiss();
    installHostAddMenu(panel);
    installAddressHistory(panel, root);
    installDownloadsMenu(panel, root);
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

  // 官方面板的「启动浏览器」按钮走的是网关 POST /start，而 task-browser 这个
  // profile 在官方配置里是 attachOnly：网关只连不拉，端点没起来时它只会回
  // Browser attachOnly is enabled and profile "task-browser" is not running，
  // 面板就永远停在空态（红字 + 那个按钮）。而那颗按钮对 attach-only profile
  // 是无解的：点它还是同一条红字。所以星舰不跟那颗按钮较劲，改成壳层接管：
  // Edge 以 headless 拉起，只提供 CDP 目标，用户看到的画面仍然是壳层自己的
  // 原生 WebView2 子视图。三层配合：
  //   1) 面板一出现先预热一次，端点通常在这一步就绪；
  //   2) 盖住控制器的 setState：attach-only 那条报错根本不上屏（它只是「端点
  //      晚到几百毫秒」，放上去只会让用户以为浏览器坏了），改触发自愈；
  //   3) 端点起来后用面板自己的 refreshAll 让它重查一次网关，页面自己出来。
  var BROWSER_ENSURE_TIMEOUT_MS = 15000;
  // 端点是一个随壳层存活的本地进程，正常路径是「面板一出现就补一次」；下面这
  // 个节奏只覆盖补失败、或者端点中途掉了的情况，不影响健康态（健康态一次也不探）。
  var BROWSER_HEAL_INTERVAL_MS = 1200;
  var BROWSER_HEAL_COOLDOWN_MS = 2000;
  var BROWSER_HEAL_MAX_COOLDOWN_MS = 8000;
  // 端点静默死亡（chrome 被系统回收、被用户关掉、崩溃）时面板不会立刻求救：
  // 它显示的是壳层自己的原生视图，看着一切正常，直到下一次有人来问网关。所以
  // 除了「面板求救时自愈」，还需要一个低频看门狗把端点维持住，否则用户点一下
  // 标签、或让星魂用一次浏览器，就会撞上那条红字。端口活着时这一趟只是本地
  // connect 探测，零副作用。
  var BROWSER_WATCHDOG_MS = 10000;
  var BROWSER_WATCHDOG_BACKOFF_MS = 60000;
  var BROWSER_WATCHDOG_BACKOFF_AFTER = 3;
  // 官方把网关的原始错误原样拼进这条提示，attach-only profile 未运行是唯一一种
  // 壳层能自己解决、而且必然能解决的失败，所以只认这一条，其余报错照常上屏。
  var BROWSER_ATTACH_ERROR = /attachOnly is enabled and profile [^]* is not running/i;
  var browserEnsureInFlight = null;
  var browserHealAt = 0;
  var browserWatchdogAt = 0;
  var browserWatchdogFailures = 0;
  // 连续几次都拉不起端点，说明这不是「晚到几百毫秒」，而是真的坏了。这时候
  // 藏着错误只会让用户面对一块永远空着的面板，所以从第 BROWSER_SURFACE_AFTER
  // 次失败开始，把官方那条原文放回界面上。
  var BROWSER_SURFACE_AFTER = 3;
  var browserEnsureFailures = 0;
  // 端点归壳层管（配置里就是 attachOnly 的本地回环 profile）时才自愈；否则一次
  // 都不碰——网关自己管的浏览器、Chrome 扩展、远端 CDP 都不是壳层的事。
  var browserHealOwned = true;

  function ensureShellBrowser() {
    if (browserEnsureInFlight) {
      return browserEnsureInFlight;
    }
    browserEnsureInFlight = postMessage({ type: "ensure-browser" }).then(
      function (reply) {
        browserEnsureInFlight = null;
        if (reply && reply.owned === false) {
          // profile 不归壳层管：这次以后不再补、也不再让面板重查。
          browserHealOwned = false;
        }
        var ok = !!(reply && reply.ok);
        browserEnsureFailures = ok ? 0 : browserEnsureFailures + 1;
        return ok;
      },
      function () {
        browserEnsureInFlight = null;
        browserEnsureFailures += 1;
        return false;
      },
    );
    return browserEnsureInFlight;
  }

  // 让面板重查一次网关。这是官方面板自己的入口，走的是它自己的 client，所以
  // 不碰它的任何内部数据结构，只把「再问一次」这件事推进去。
  function refreshBrowserPanel(controller) {
    var now = Date.now();
    var delay = controller.__starshipHealDelay || BROWSER_HEAL_COOLDOWN_MS;
    if (controller.__starshipHealAt && now - controller.__starshipHealAt < delay) {
      return;
    }
    controller.__starshipHealAt = now;
    // 反复失败时指数退避：端点真起不来时，别把这个重试变成 1 秒一次的水泵。
    controller.__starshipHealDelay = Math.min(
      delay * 2,
      BROWSER_HEAL_MAX_COOLDOWN_MS,
    );
    try {
      controller.refreshAll();
    } catch (error) {
      // The chrome layer must never take the official panel down with it.
    }
  }

  // 面板处于「壳层该出手」的状态：带着 attach-only 那条报错，或者网关侧浏览器
  // 没起来（空态那颗「启动浏览器」按钮的条件）。已经拿到原生标签页的面板不算。
  function pendingBrowserPanel(controller) {
    if (!controller || !controller.native) {
      return false;
    }
    if (controller.native.activeTab) {
      controller.__starshipHealDelay = 0;
      return false;
    }
    if (
      typeof controller.errorText === "string" &&
      BROWSER_ATTACH_ERROR.test(controller.errorText)
    ) {
      return true;
    }
    return controller.running !== true;
  }

  // 壳层接管的全部动作：先保证端点，再让停在空态/错误态的面板重查一次。用户
  // 看到的是「打开就有页面」，而不是「先红一行字、再点一次那个按钮」。
  function healBrowserPanels() {
    if (!browserHealOwned) {
      return;
    }
    var panels = document.querySelectorAll(PANEL_SELECTOR);
    if (!panels.length) {
      return;
    }
    var targets = [];
    for (var index = 0; index < panels.length; index += 1) {
      var controller = panels[index].browserPanelController;
      if (pendingBrowserPanel(controller)) {
        targets.push(controller);
      }
    }
    if (!targets.length) {
      return;
    }
    var now = Date.now();
    if (now - browserHealAt < BROWSER_HEAL_INTERVAL_MS) {
      return;
    }
    browserHealAt = now;
    ensureShellBrowser().then(
      function (started) {
        if (!started) {
          return;
        }
        for (var index = 0; index < targets.length; index += 1) {
          refreshBrowserPanel(targets[index]);
        }
      },
      function () {
        /* 端点没起来：面板自己会再试，这里不抛。 */
      },
    );
  }

  // 端点归壳层管、且界面上确实挂着一个浏览器面板时，每隔 BROWSER_WATCHDOG_MS
  // 摸一次端口。健康时 ensureShellBrowser 只是一次本地 connect，没有任何动作；
  // 连续起不来就退到分钟级，避免把一个注定失败的拉起变成后台水泵。
  function watchdogShellBrowser() {
    if (!browserHealOwned) {
      return;
    }
    if (!document.querySelector(PANEL_SELECTOR)) {
      return;
    }
    var now = Date.now();
    var wait =
      browserWatchdogFailures >= BROWSER_WATCHDOG_BACKOFF_AFTER
        ? BROWSER_WATCHDOG_BACKOFF_MS
        : BROWSER_WATCHDOG_MS;
    if (now - browserWatchdogAt < wait) {
      return;
    }
    browserWatchdogAt = now;
    ensureShellBrowser().then(
      function (ok) {
        browserWatchdogFailures = ok ? 0 : browserWatchdogFailures + 1;
      },
      function () {
        browserWatchdogFailures += 1;
      },
    );
  }

  // ── 弹窗新标签的接管 + present 闩看门狗 ────────────────────────────────────
  // 两个症状同一个根。官方面板的 present 走 requestAnimationFrame：窗口最小化
  // 或被别的窗口盖住时渲染器停掉 rAF，`frame` 这根闩就永远停在非 null，之后
  // 每次 schedule() 都在门口短路 —— 面板再也报不出「屏幕上显示的是哪个标签」。
  // 而官方「native 新标签自动切前台」的判定要读 presenter 的 presentedTabId /
  // lastPresented，present 发不出去时判定必然落空：用户点一个 target=_blank 的
  // 链接，新标签停在后台、标签行也不切，只剩壳层探针兜底把新页面硬顶到屏幕上，
  // 看起来就是「点一下跳一下」。这里在注入层补两件事，官方 dist 一行不改：
  //   1. present 闩看门狗：超时还没被 rAF 清掉就自己清，窗口可见时立刻补报一次。
  //   2. 弹窗接管：state 里出现 `openedBy === "native"` 的新标签就直接选中它。
  var BROWSER_FRAME_WATCHDOG_MS = 160;
  var browserPresentations = [];
  function patchPresentationSchedule(presentation) {
    if (!presentation || presentation.__starshipFrameWatchdog) {
      return;
    }
    if (typeof presentation.schedule !== "function") {
      return;
    }
    presentation.__starshipFrameWatchdog = true;
    browserPresentations.push(presentation);
    // 官方把 schedule 定义成实例字段（箭头函数），这里也在实例上盖一层。
    var rawSchedule = presentation.schedule;
    presentation.schedule = function () {
      var result = rawSchedule.apply(this, arguments);
      var self = this;
      if (self.__starshipFrameTimer) {
        window.clearTimeout(self.__starshipFrameTimer);
        self.__starshipFrameTimer = 0;
      }
      var armed = self.frame;
      if (armed === null || armed === undefined) {
        return result;
      }
      self.__starshipFrameTimer = window.setTimeout(function () {
        self.__starshipFrameTimer = 0;
        // rAF 已经跑过就会换成新的一根闩，不用管；仍是原值说明渲染器停了。
        if (self.frame !== armed) {
          return;
        }
        self.frame = null;
        // 窗口不可见时 report() 量到的是 0×0 / 命中不到面板，会走 hide()，把
        // 已经上屏的原生视图一起收掉。这里先只解闩，等窗口回来再补报。
        if (document.visibilityState !== "visible") {
          return;
        }
        try {
          self.report();
        } catch (error) {
          /* The chrome layer must never take the official panel down with it. */
        }
      }, BROWSER_FRAME_WATCHDOG_MS);
      return result;
    };
  }
  // 最小化期间被闩住的 present 得在窗口回来时补一次；官方自己不监听这个事件。
  function flushHeldPresentations() {
    if (document.visibilityState !== "visible") {
      return;
    }
    for (var index = 0; index < browserPresentations.length; index += 1) {
      var presentation = browserPresentations[index];
      if (!presentation || !presentation.connected) {
        continue;
      }
      if (presentation.frame !== null && presentation.frame !== undefined) {
        try {
          window.cancelAnimationFrame(presentation.frame);
        } catch (error) {
          /* A stale handle only means there is nothing left to cancel. */
        }
        presentation.frame = null;
      }
      try {
        presentation.schedule();
      } catch (error) {
        /* The chrome layer must never take the official panel down with it. */
      }
    }
  }
  document.addEventListener("visibilitychange", flushHeldPresentations);
  window.addEventListener("focus", flushHeldPresentations);
  // 一块面板是否挂在屏幕上那块 pane 上。cached pane 里也各有一块面板，
  // 它们收得到同一份 state，但不该去抢标签的激活权。
  function browserPanelIsLive(panel) {
    var pane =
      panel && typeof panel.closest === "function"
        ? panel.closest("openclaw-chat-pane")
        : null;
    if (pane && !pane.classList.contains(LIVE_PANE_CLASS)) {
      return false;
    }
    if (typeof panel.checkVisibility === "function") {
      try {
        return panel.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true });
      } catch (error) {
        return true;
      }
    }
    return panel.offsetParent !== null;
  }
  // 一个弹窗只能有一个面板接管。官方的归属判定读 presentedTabId，present 卡住
  // 时它就废了；这里按同一套优先级重挑一次：先判给正在显示 opener 的那块面板，
  // 再退到「屏幕上的、最近 present 过的那块」。
  function popupPanelOwner(controller, tab) {
    var panels = document.querySelectorAll(PANEL_SELECTOR);
    var mine = null;
    var openerOwner = null;
    var liveOwner = null;
    for (var index = 0; index < panels.length; index += 1) {
      var other = panels[index].browserPanelController;
      if (!other || !other.native || !other.native.presentation) {
        continue;
      }
      if (other === controller) {
        mine = other;
      }
      if (
        typeof tab.openerTabId === "string" &&
        other.native.presentation.presentedTabId === tab.openerTabId
      ) {
        openerOwner = other;
      }
      var presented = other.native.presentation.lastPresented || 0;
      if (
        browserPanelIsLive(panels[index]) &&
        (liveOwner === null || presented > (liveOwner.native.presentation.lastPresented || 0))
      ) {
        liveOwner = other;
      }
    }
    if (!mine) {
      return null;
    }
    return openerOwner || liveOwner;
  }
  // 官方只在「推送」这条路上自动切前台，而且要求有 presenter 归属。这里把同一
  // 条链路补成确定性的：state 里冒出一个 `openedBy === "native"` 的新标签时，
  // 直接让屏幕上那块面板选中它，不再等 present 归属。
  function patchNativeTabActivation(controller) {
    var native = controller.native;
    if (!native || native.__starshipPopupHook || typeof native.acceptState !== "function") {
      return;
    }
    native.__starshipPopupHook = true;
    var rawAcceptState = native.acceptState;
    native.acceptState = function (state, activatePopups) {
      var known = {};
      var knownCount = 0;
      try {
        var existing = native.tabs || [];
        for (var index = 0; index < existing.length; index += 1) {
          known[existing[index].id] = true;
          knownCount += 1;
        }
      } catch (error) {
        knownCount = 0;
      }
      var result = rawAcceptState.apply(this, arguments);
      try {
        // 首帧快照（activatePopups === false）和面板刚挂上、还没有任何已知标签
        // 的时候都不接管，免得重挂面板把焦点抢到一个旧标签上。壳层点名的那条
        // 请求（focusTabId）是例外：它是「刚刚发生」的一次动作，不是重挂时的
        // 陈旧状态。
        var tabs = state && state.tabs;
        if (!Array.isArray(tabs)) {
          return result;
        }
        // 壳层点名要请到前台的标签：站点反复 `window.open` 同一个地址时，壳层
        // 复用已经开着的那个标签而不是再复制一份，官方面板只在「第一次见到的
        // native 标签」上自动切，已有标签它接不住，所以由这一层照办。
        var focusId = typeof state.focusTabId === "string" ? state.focusTabId : "";
        if ((!activatePopups || knownCount === 0) && !focusId) {
          return result;
        }
        var target = null;
        for (var index = 0; index < tabs.length; index += 1) {
          var tab = tabs[index];
          if (!tab || typeof tab.id !== "string") {
            continue;
          }
          if (focusId && tab.id === focusId) {
            target = tab;
            break;
          }
          if (tab.openedBy !== "native" || known[tab.id]) {
            continue;
          }
          target = tab;
        }
        if (!target || controller.activeTargetId === target.id) {
          return result;
        }
        if (popupPanelOwner(controller, target) !== controller) {
          return result;
        }
        var selected = controller.selectTab(target.id);
        if (selected && typeof selected.catch === "function") {
          selected.catch(function () {
            /* 选中失败时官方自己那条兜底链路还在，这里不抛。 */
          });
        }
      } catch (error) {
        /* The chrome layer must never take the official panel down with it. */
      }
      return result;
    };
  }

  // setState 是官方面板控制器的唯一写入口，errorText 也是从这里进 state 的。
  // 在实例上盖一层：只拦 attach-only 那一条，其余状态原样透传。
  //
  // 为什么连红字都不让它上屏：这条错误是「端点还没起来」，而端点是壳层自己拉
  // 的，几百毫秒后就绪。放它上屏只有一个后果——用户以为浏览器坏了，然后去点那
  // 颗「启动浏览器」（网关对 attach-only profile 一律拒绝，点了还是同一条红字）。
  // 但如果连续几次都没拉起来（浏览器被卸了、路径变了），藏错误就等于把用户丢在
  // 一块空面板前面，所以那种情况下原样放行，见下方 BROWSER_SURFACE_AFTER。
  function hookBrowserPanel(panel) {
    var controller = panel && panel.browserPanelController;
    if (
      !controller ||
      controller.__starshipBrowserHook ||
      typeof controller.setState !== "function"
    ) {
      return;
    }
    controller.__starshipBrowserHook = true;
    patchPresentationSchedule(controller.native && controller.native.presentation);
    patchNativeTabActivation(controller);
    var setState = controller.setState;
    controller.setState = function (key, value) {
      if (
        key === "errorText" &&
        typeof value === "string" &&
        BROWSER_ATTACH_ERROR.test(value) &&
        browserHealOwned &&
        browserEnsureFailures < BROWSER_SURFACE_AFTER
      ) {
        healBrowserPanels();
        return;
      }
      return setState.apply(this, arguments);
    };
  }

  function scanPanels() {
    parityPending = null;
    var panels = document.querySelectorAll(PANEL_SELECTOR);
    // 拦截器要盖在官方任何一次 setState 之前，所以先挂钩再谈几何。
    for (var index = 0; index < panels.length; index += 1) {
      try {
        hookBrowserPanel(panels[index]);
      } catch (error) {
        // The chrome layer must never take the official panel down with it.
      }
    }
    healBrowserPanels();
    watchdogShellBrowser();
    if (!parityEnabled()) {
      return;
    }
    for (var index = 0; index < panels.length; index += 1) {
      try {
        installParity(panels[index]);
      } catch (error) {
        // The chrome layer must never take the official panel down with it.
      }
    }
  }
  // 两种节奏：面板**刚插进 DOM**（开面板、切面板类型、pane 重挂载）走下一帧，
  // 免得官方 present 先带着「还没合并」的几何上屏再被纠正；只是 class / active
  // 这类高频抖动仍走 200ms 防抖，避免每次 class 抖动都重算全部面板的几何。
  function scheduleParityScan(immediate) {
    if (parityPending !== null) {
      if (!immediate) { return; }
      // 已经排了一次慢扫描，现在来了结构性变化，把它提到下一帧。
      window.clearTimeout(parityPending);
      window.cancelAnimationFrame(parityPending);
      parityPending = null;
    }
    parityPending = immediate
      ? window.setTimeout(scanPanels, 16)
      : window.setTimeout(scanPanels, 200);
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
      new MutationObserver(function (records) {
        // 结构性变化（节点新增/移除）优先：开面板那一刻官方马上就会 present 一次
        // 它自己的几何，星舰必须抢在用户看见之前把标签行抬上去。
        for (var index = 0; index < records.length; index += 1) {
          var record = records[index];
          var added = record.addedNodes;
          if (record.type !== "childList" || !added || !added.length) { continue; }
          for (var node = 0; node < added.length; node += 1) {
            var element = added[node];
            if (!element || element.nodeType !== 1) { continue; }
            if (
              typeof element.matches !== "function"
            ) {
              continue;
            }
            // 面板自己的 connectedCallback 会立刻去问一次网关，早于下面那趟
            // 16ms 的几何扫描；挂钩必须抢在它把 attach-only 的红字写进 state 之前。
            if (element.matches(PANEL_SELECTOR)) {
              try {
                hookBrowserPanel(element);
              } catch (error) {
                /* The chrome layer must never take the official panel down. */
              }
            } else {
              var fresh = element.querySelectorAll(PANEL_SELECTOR);
              for (var each = 0; each < fresh.length; each += 1) {
                try {
                  hookBrowserPanel(fresh[each]);
                } catch (error) {
                  /* The chrome layer must never take the official panel down. */
                }
              }
            }
            if (
              element.matches(PANEL_SELECTOR) ||
              element.matches(HOST_RAIL_SELECTOR) ||
              element.matches(".side-panel__panel") ||
              element.querySelector(PANEL_SELECTOR) ||
              element.querySelector(HOST_RAIL_SELECTOR)
            ) {
              scheduleParityScan(true);
              return;
            }
          }
        }
        scheduleParityScan(false);
      }).observe(root, {
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
  // ── 冷启动不要自动展开官方「主页 / 询问OpenClaw」右栏 ──────────────────
  // 官方把右栏 assistant panel 的开合状态持久化在 localStorage 的
  // `openclaw.custodian.panel.v1`，每次启动按它恢复（dock-layout-controller 的
  // hostConnected/restoreOpenState）。星舰用自己的「星魂」，这个 onboarding 面板
  // 自动展开只会挤掉聊天宽度，所以：
  //   1. 文档最早期就把持久化状态钉成 closed —— 官方恢复逻辑读到 false 自然不开；
  //   2. 兜底看守：万一别的触发路径（minimize 请求、onboarding 流程）又把它支起来，
  //      只要用户没有主动点过官方入口，就把它关回去（hideWithoutPersisting，不写盘）。
  // 用户主动点官方那两个入口时会放行，不改官方行为。
  var ASSISTANT_LAYOUT_KEY = "openclaw.custodian.panel.v1";
  var ASSISTANT_PANEL_TAG = "openclaw-assistant-panel";
  var assistantUserOwned = false;
  var assistantLastGestureAt = 0;
  var assistantGuardTimer = null;

  function noteAssistant(message) {
    // 注入层没有回写壳层日志的通道，调试信息留在页面里给 CDP 读。
    try {
      var log = window.__starshipAssistantLog;
      if (!log) { log = []; window.__starshipAssistantLog = log; }
      log.push(Date.now() + " " + message);
      if (log.length > 80) { log.shift(); }
      console.info("[starship-assistant] " + message);
    } catch (error) {
      /* console may be detached in release builds. */
    }
  }

  function pinAssistantLayoutClosed() {
    try {
      var raw = window.localStorage.getItem(ASSISTANT_LAYOUT_KEY);
      var parsed = null;
      if (raw) { parsed = JSON.parse(raw); }
      if (!parsed || typeof parsed !== "object") { parsed = {}; }
      if (parsed.open === false) { return; }
      parsed.open = false;
      if (parsed.dock !== "right" && parsed.dock !== "bottom") { parsed.dock = "right"; }
      if (typeof parsed.height !== "number") { parsed.height = 420; }
      if (typeof parsed.width !== "number") { parsed.width = 440; }
      window.localStorage.setItem(ASSISTANT_LAYOUT_KEY, JSON.stringify(parsed));
      noteAssistant("layout pinned closed");
    } catch (error) {
      /* localStorage may be unavailable while the shell is tearing down. */
    }
  }

  // 只钉住初始状态还不够：官方在「restoreOpenState」和 minimize/onboarding 路径里
  // 都会把 open:true 再写回去。把写入本身拦住，官方恢复时读到的永远是 closed。
  function guardAssistantStorage() {
    try {
      var proto = Object.getPrototypeOf(window.localStorage);
      var original = proto.setItem;
      if (typeof original !== "function") { return; }
      proto.setItem = function (key, value) {
        if (key === ASSISTANT_LAYOUT_KEY && !assistantUserOwned) {
          try {
            var parsed = JSON.parse(value);
            if (parsed && typeof parsed === "object" && parsed.open !== false) {
              parsed.open = false;
              value = JSON.stringify(parsed);
              noteAssistant("layout write forced closed");
            }
          } catch (error) {
            /* 非 JSON 的写入按原样放行。 */
          }
        }
        return original.call(this, key, value);
      };
    } catch (error) {
      /* localStorage 在壳层拆除阶段可能已不可用。 */
    }
  }

  function noteAssistantGesture() {
    assistantLastGestureAt = Date.now();
  }

  function noteAssistantIntent(event) {
    // 官方顶栏那两个入口派发这两个事件。但这两个事件也可能是程序在启动流程里
    // 派发的 —— 那正是要拦的情况，所以只有「刚刚真的有键鼠操作」才算用户意图。
    var detail = null;
    try { detail = event && event.detail ? event.detail : null; } catch (error) { detail = null; }
    var wantedClosed = detail && detail.open === false;
    if (wantedClosed) {
      assistantUserOwned = false;
      noteAssistant("user closed via " + event.type);
      return;
    }
    if (Date.now() - assistantLastGestureAt < 1500) {
      assistantUserOwned = true;
      noteAssistant("user opened via " + event.type);
      return;
    }
    noteAssistant("programmatic " + event.type + " ignored");
  }

  function findAssistantPanel(root, depth) {
    if (!root || depth > 30) { return null; }
    var direct = null;
    try { direct = root.querySelector(ASSISTANT_PANEL_TAG); } catch (error) { direct = null; }
    if (direct) { return direct; }
    var hosts;
    try { hosts = root.querySelectorAll("*"); } catch (error) { return null; }
    for (var index = 0; index < hosts.length; index += 1) {
      var host = hosts[index];
      if (!host.shadowRoot) { continue; }
      var nested = findAssistantPanel(host.shadowRoot, depth + 1);
      if (nested) { return nested; }
    }
    return null;
  }

  // 官方 shell 把面板放在自己的 shadow root 里，`document.querySelector` 穿不过去，
  // 而每 300ms 全树遍历一次太贵。所以找到一次就记住引用，只在引用失效时重找。
  var assistantPanelRef = null;
  function resolveAssistantPanel() {
    if (assistantPanelRef && assistantPanelRef.isConnected) { return assistantPanelRef; }
    assistantPanelRef = findAssistantPanel(document, 0);
    return assistantPanelRef;
  }

  function closeAssistantPanel(panel) {
    // 先钉住持久化状态再收面板：面板收起后官方内部的 willUpdate 会走
    // restoreOpenState()，它读的就是 localStorage，先钉住就不会被自己反弹回来。
    pinAssistantLayoutClosed();
    var layout = panel.dockLayout;
    if (layout && typeof layout.hideWithoutPersisting === "function") {
      layout.hideWithoutPersisting();
      return true;
    }
    if (layout && typeof layout.setOpen === "function") {
      layout.setOpen(false, false);
      return true;
    }
    if (typeof panel.setOpen === "function") {
      panel.setOpen(false);
      return true;
    }
    // 自定义元素还没被官方 chunk 升级（没有 dockLayout / setOpen）时，唯一
    // 还能走的就是官方自己监听的两个顶栏事件。`detail.open === false` 是官方
    // 约定的「关闭」信号，destination 对不上时它自己会忽略，两个都发一遍即可。
    try {
      window.dispatchEvent(
        new CustomEvent("openclaw:assistant-toggle", { detail: { open: false } }),
      );
      window.dispatchEvent(
        new CustomEvent("openclaw:home-toggle", { detail: { open: false } }),
      );
      return true;
    } catch (error) {
      /* CustomEvent 在所有目标 WebView2 运行时可用的兜底。 */
    }
    return false;
  }

  function guardAssistantPanel() {
    pinAssistantLayoutClosed();
    var panel = resolveAssistantPanel();
    if (!panel) { return; }
    // 宿主自定义元素是 `position:fixed` 的零尺寸盒子（真正渲染的 section 在里面
    // 也是 fixed），所以量宿主 rect 永远是 0×0。可信的可见性判据是官方自己的
    // dock 状态：open 且当前 destination 可用，就等于面板已经占住屏幕右侧。
    var layout = panel.dockLayout;
    var opened = !!(layout && layout.open === true);
    if (!layout && panel.assistantPanelOpen === true) { opened = true; }
    var available = !!(panel.homeAvailable || panel.custodianAvailable);
    if (!opened || !available) {
      // 面板收起来了，用户对这一轮的「拥有权」结束。
      assistantUserOwned = false;
      return;
    }
    // 用户自己点开的面板不打扰，直到他关掉为止。
    if (assistantUserOwned) { return; }
    if (closeAssistantPanel(panel)) {
      noteAssistant(
        "assistant panel auto-closed dest=" + panel.destination,
      );
    }
  }

  document.addEventListener("pointerdown", noteAssistantGesture, true);
  document.addEventListener("keydown", noteAssistantGesture, true);
  window.addEventListener("openclaw:home-toggle", noteAssistantIntent, true);
  window.addEventListener("openclaw:assistant-toggle", noteAssistantIntent, true);
  pinAssistantLayoutClosed();
  guardAssistantStorage();
  // 启动阶段是它最容易自己支起来的时候，前 20 秒用高频看守，之后降频。
  var assistantGuardTicks = 0;
  assistantGuardTimer = window.setInterval(function () {
    assistantGuardTicks += 1;
    guardAssistantPanel();
    if (assistantGuardTicks === 66) {
      window.clearInterval(assistantGuardTimer);
      assistantGuardTimer = window.setInterval(guardAssistantPanel, 2000);
    }
  }, 300);

  // ── 地址栏历史下拉（Codex 形态） ───────────────────────────────────────────
  // 官方地址栏只是一个「填 URL 按回车」的输入框：没有历史、没有联想，关掉客户端
  // 什么都不记得。Codex 的地址栏点开是一列「favicon + 标题 + 域名」，最下面还有
  // 一条当前页。星舰补的就是这一列 —— 数据来自壳层的 `browser-history.json`
  // （原生导航事件在壳层落盘），所以它活过重启，也不受官方 UI 重渲染影响。
  var ADDR_HISTORY_ASK_LIMIT = 60;
  var ADDR_HISTORY_ROWS = 12;
  // 拉一次要过一趟 IPC（首屏还会顺手补几个站点图标），所以同一次交互里复用几秒内
  // 的结果，不让 focus / 打字把这条通道抽成水泵。
  var ADDR_HISTORY_TTL_MS = 4000;
  var addrHistoryEntries = [];
  var addrHistoryAt = 0;
  var addrHistoryInFlight = null;
  var addrMenuRoot = null;
  var addrMenuIndex = -1;
  // 最近一次下拉是用哪个查询串填的；异步回包靠它判断自己是不是过期了。
  var addrMenuQuery = "";

  // 下拉层的排查日志：验收时要能从控制台直接看「开过几次、拿到几条」。
  window.__starshipAddrLog = window.__starshipAddrLog || [];
  function noteAddress(line) {
    window.__starshipAddrLog.push(String(line));
    if (window.__starshipAddrLog.length > 50) { window.__starshipAddrLog.shift(); }
  }
  function hostOf(url) {
    try {
      return new URL(url).host;
    } catch (error) {
      return "";
    }
  }
  // `www.` 对用户没有信息量，列表里一律去掉，宽出来的位置留给路径。
  function hostLabel(host) {
    return String(host || "").replace(/^www\./i, "");
  }
  // 同一个域名永远同一个色，抓不到图标的站点退化成首字母色块时不会一刷新就换颜色。
  function hostTint(host) {
    var text = hostLabel(host);
    var hash = 0;
    for (var index = 0; index < text.length; index += 1) {
      hash = (hash * 31 + text.charCodeAt(index)) % 360;
    }
    return "hsl(" + hash + ", 44%, 40%)";
  }
  function shortAddress(url) {
    try {
      var parsed = new URL(url);
      var tail = parsed.pathname === "/" ? "" : parsed.pathname + parsed.search;
      return hostLabel(parsed.host) + tail;
    } catch (error) {
      return String(url || "");
    }
  }
  // 打字即筛选：Codex 的地址栏是「输入就收窄列表」，官方那只是「填完按回车」。
  // 先把用户输入归一化 —— 去掉协议头（`https://` 一进来会把整屏都匹配上）、
  // 去掉 `www.`、去掉尾斜杠，剩下的才拿去做子串比较。
  function normalizeAddressQuery(value) {
    var text = String(value === undefined || value === null ? "" : value).trim().toLowerCase();
    text = text.replace(/^[a-z][a-z0-9+.-]*:\/\//, "");
    text = text.replace(/^www\./, "");
    return text.replace(/\/+$/, "");
  }
  function addressMatches(entry, query) {
    if (!query) { return true; }
    var url = String(entry.url || "");
    var host = hostOf(url).toLowerCase();
    var title = String(entry.title || "").toLowerCase();
    return (
      host.indexOf(query) >= 0 ||
      title.indexOf(query) >= 0 ||
      url.toLowerCase().indexOf(query) >= 0
    );
  }
  function requestAddressHistory() {
    var now = Date.now();
    if (
      addrHistoryEntries.length &&
      now - addrHistoryAt < ADDR_HISTORY_TTL_MS
    ) {
      return Promise.resolve(addrHistoryEntries);
    }
    if (addrHistoryInFlight) { return addrHistoryInFlight; }
    addrHistoryInFlight = postMessage({
      type: "history",
      limit: ADDR_HISTORY_ASK_LIMIT,
    }).then(
      function (reply) {
        addrHistoryInFlight = null;
        addrHistoryEntries = reply && reply.ok && reply.entries ? reply.entries : [];
        addrHistoryAt = Date.now();
        return addrHistoryEntries;
      },
      function () {
        addrHistoryInFlight = null;
        return addrHistoryEntries;
      },
    );
    return addrHistoryInFlight;
  }
  function addressMenu(root) {
    return root ? root.querySelector(".starship-addr__menu") : null;
  }
  function addressInput(root) {
    var toolbar = root.querySelector(".bp-toolbar");
    return toolbar ? toolbar.querySelector(".bp-url") : null;
  }
  // 下拉贴着地址栏：左边对齐输入框，宽度取输入框宽度，右边不够就收窄，别顶出窗口。
  function placeAddressMenu(root, menu) {
    var input = addressInput(root);
    if (!input) { return false; }
    var rect = input.getBoundingClientRect();
    if (rect.width < 2) { return false; }
    var width = Math.max(rect.width, 320);
    var maxLeft = window.innerWidth - width - 10;
    if (maxLeft < 8) {
      width = Math.max(window.innerWidth - 16, 280);
      maxLeft = 8;
    }
    menu.style.left = Math.min(Math.max(rect.left, 8), Math.max(maxLeft, 8)) + "px";
    menu.style.top = rect.bottom + 4 + "px";
    menu.style.width = width + "px";
    return true;
  }
  function syncActiveAddressRow(menu) {
    var rows = menu.querySelectorAll(".starship-addr__row");
    for (var index = 0; index < rows.length; index += 1) {
      if (index === addrMenuIndex) {
        rows[index].setAttribute("data-active", "1");
      } else {
        rows[index].removeAttribute("data-active");
      }
    }
    var active = rows[addrMenuIndex];
    if (active && typeof active.scrollIntoView === "function") {
      // 键盘走到列表外的行时要把它带进视野，滚动范围只限下拉自己。
      active.scrollIntoView({ block: "nearest" });
    }
  }
  function closeAddressMenu(root) {
    var ownerRoot = root || addrMenuRoot;
    if (!ownerRoot) { return; }
    var menu = addressMenu(ownerRoot);
    addrMenuRoot = null;
    addrMenuIndex = -1;
    if (!menu || menu.hidden) { return; }
    menu.hidden = true;
    noteAddress("closed");
  }
  function addressRow(entry) {
    var row = document.createElement("button");
    row.type = "button";
    row.className = "starship-addr__row";
    row.setAttribute("role", "option");
    var host = hostOf(entry.url);
    var icon = typeof entry.favicon === "string" ? entry.favicon : "";
    if (icon.indexOf("data:image/") === 0) {
      var image = document.createElement("img");
      image.className = "starship-addr__icon";
      image.alt = "";
      image.src = icon;
      // 图标解不开（缓存里的 data URL 被截断之类）就退回首字母色块，别留个破图。
      image.addEventListener("error", function () {
        var fallback = addressGlyph(host);
        if (image.parentNode) { image.parentNode.replaceChild(fallback, image); }
      });
      row.appendChild(image);
    } else {
      row.appendChild(addressGlyph(host));
    }
    var title = document.createElement("span");
    title.className = "starship-addr__title";
    title.textContent = entry.title || shortAddress(entry.url);
    var label = document.createElement("span");
    label.className = "starship-addr__host";
    label.textContent = hostLabel(host) || String(entry.url || "");
    row.appendChild(title);
    row.appendChild(label);
    row.__starshipUrl = entry.url;
    return row;
  }
  function addressGlyph(host) {
    var glyph = document.createElement("span");
    glyph.className = "starship-addr__glyph";
    glyph.style.background = hostTint(host);
    var text = hostLabel(host);
    glyph.textContent = (text.charAt(0) || "?").toUpperCase();
    return glyph;
  }
  function navigateFromAddress(panel, root, url) {
    closeAddressMenu(root);
    // 跳完这一页，地址栏里重新写的是「当前页」而不是用户草稿，筛选得关掉。
    root.__starshipAddrTyped = false;
    // 这一跳会改写历史，下次打开下拉就得重新问一次壳层。
    addrHistoryAt = 0;
    var input = addressInput(root);
    if (input) {
      // 只赋值、不派发事件：官方那份草稿自己会在导航回来后刷新，这里先让地址栏
      // 立刻显示目标地址，用户不会看到「点了没反应」。
      try { input.value = url; } catch (error) { /* 面板重渲染会覆盖，无妨。 */ }
    }
    noteAddress("navigate " + url);
    // 面板上没有活动标签时（刚打开面板、标签被关光、官方刚重挂过），`actOnPanel`
    // 连消息都发不出去：它读 `controller.activeTargetId`，为 null 就直接 resolve
    // 一个失败。用户看到的是「历史项点了、回车按了，浏览器不出来」。这种情况退回
    // 壳层的 `open`，让它建一个新标签把这一页装进去。
    if (!panelTabId(panel)) {
      noteAddress("open " + url + " (no active tab)");
      postMessage({ type: "open", url: url });
      return;
    }
    actOnPanel(panel, "navigate", { url: url });
  }
  function openAddressMenu(panel, root, rawQuery) {
    var menu = addressMenu(root);
    if (!menu) { return; }
    addrMenuRoot = root;
    addrMenuIndex = -1;
    var input = addressInput(root);
    var current = input ? String(input.value || "") : "";
    // 聚焦时列表给全量（此刻地址栏里写的是当前页，拿它去筛只会筛出空列表）；
    // 只有 input 事件才带查询串进来收窄。
    var query = rawQuery === undefined ? "" : normalizeAddressQuery(rawQuery);
    addrMenuQuery = query;
    if (menu.hidden) {
      menu.hidden = false;
      menu.innerHTML = "";
      var loading = document.createElement("div");
      loading.className = "starship-addr__empty";
      loading.textContent = "\u6b63\u5728\u8bfb\u53d6\u5386\u53f2\u8bb0\u5f55\u2026";
      menu.appendChild(loading);
      if (!placeAddressMenu(root, menu)) {
        closeAddressMenu(root);
        return;
      }
      noteAddress("opened");
    }
    requestAddressHistory().then(function (entries) {
      if (addrMenuRoot !== root || menu.hidden) { return; }
      // 异步回来的时候用户可能又敲了两下，当前的查询串已经不是发起时那个了，
      // 这一份结果就作废，等最新那次调用自己填。
      if (query !== (addrMenuQuery === undefined ? "" : addrMenuQuery)) { return; }
      menu.innerHTML = "";
      var shown = 0;
      for (var index = 0; index < entries.length; index += 1) {
        var entry = entries[index];
        if (!entry || !entry.url) { continue; }
        // 当前页已经在地址栏里写着，列表里不再占一行。
        if (current && entry.url === current) { continue; }
        if (!addressMatches(entry, query)) { continue; }
        if (shown >= ADDR_HISTORY_ROWS) { break; }
        menu.appendChild(addressRow(entry));
        shown += 1;
      }
      if (!shown) {
        var empty = document.createElement("div");
        empty.className = "starship-addr__empty";
        empty.textContent = query
          ? "\u6ca1\u6709\u5339\u914d\u7684\u6d4f\u89c8\u8bb0\u5f55"
          : "\u8fd8\u6ca1\u6709\u6d4f\u89c8\u8bb0\u5f55";
        menu.appendChild(empty);
      }
      if (current) {
        var foot = document.createElement("div");
        foot.className = "starship-addr__foot";
        foot.textContent = shortAddress(current);
        menu.appendChild(foot);
      }
      syncActiveAddressRow(menu);
      placeAddressMenu(root, menu);
      noteAddress(
        "filled rows=" + shown + " total=" + entries.length +
        (query ? " query=" + query : "")
      );
    });
  }
  // 点外面关掉。和菜单那套一样挂 document：官方面板是 Lit 渲染的，挂在面板内部
  // 节点上的监听会随重渲染一起消失。
  function installAddressDismiss() {
    if (document.__starshipAddrDismiss) { return; }
    document.__starshipAddrDismiss = true;
    document.addEventListener(
      "pointerdown",
      function (event) {
        var path = typeof event.composedPath === "function" ? event.composedPath() : [];
        for (var index = 0; index < path.length; index += 1) {
          var node = path[index];
          if (!node || typeof node.closest !== "function") { continue; }
          if (node.closest(".starship-addr__menu, .bp-url")) { return; }
        }
        closeAddressMenu(null);
      },
      true,
    );
    window.addEventListener("resize", function () {
      if (!addrMenuRoot) { return; }
      var menu = addressMenu(addrMenuRoot);
      if (!menu || menu.hidden) { addrMenuRoot = null; return; }
      placeAddressMenu(addrMenuRoot, menu);
    });
  }
  function installAddressHistory(panel, root) {
    var toolbar = root.querySelector(".bp-toolbar");
    if (!toolbar || !toolbar.querySelector(".bp-url")) { return false; }
    var menu = addressMenu(root);
    if (!menu) {
      menu = document.createElement("div");
      menu.className = "starship-addr__menu";
      menu.setAttribute("role", "listbox");
      // 这一个属性就是「别被网页盖住」的全部机关：遮挡探测把它当浮层，壳层随即
      // 把原生子视图换成同位置的截图，下拉才能压住页面。
      menu.setAttribute("data-starship-overlay", "1");
      menu.hidden = true;
      // 按在下拉上时不让焦点跑掉：输入框一失焦，官方面板就会把列表收起来。
      menu.addEventListener("pointerdown", function (event) { event.preventDefault(); });
      menu.addEventListener("pointerover", function (event) {
        var row = event.target && typeof event.target.closest === "function"
          ? event.target.closest(".starship-addr__row")
          : null;
        if (!row) { return; }
        var rows = menu.querySelectorAll(".starship-addr__row");
        addrMenuIndex = Array.prototype.indexOf.call(rows, row);
        syncActiveAddressRow(menu);
      });
      menu.addEventListener("click", function (event) {
        var row = event.target && typeof event.target.closest === "function"
          ? event.target.closest(".starship-addr__row")
          : null;
        if (!row || !row.__starshipUrl) { return; }
        event.preventDefault();
        event.stopPropagation();
        navigateFromAddress(panel, root, row.__starshipUrl);
      });
      root.appendChild(menu);
    }
    installAddressDismiss();
    // 工具行可能被官方整个换掉，监听要跟着挪；同一个工具行只挂一次。
    if (root.__starshipAddrToolbar === toolbar) { return true; }
    root.__starshipAddrToolbar = toolbar;
    toolbar.addEventListener(
      "focusin",
      function (event) {
        if (event.target !== addressInput(root)) { return; }
        // 刚聚焦时地址栏里是当前页地址，不是用户在搜 —— 列表给全量。
        root.__starshipAddrTyped = false;
        openAddressMenu(panel, root);
      },
      true,
    );
    toolbar.addEventListener(
      "input",
      function (event) {
        if (event.target !== addressInput(root)) { return; }
        // 输入即筛选：把刚敲进去的内容当查询串重开一次列表。
        root.__starshipAddrTyped = true;
        openAddressMenu(panel, root, event.target.value);
      },
      true,
    );
    // 捕获阶段挂在工具行上：官方面板在输入框自己身上也听 Enter，同级监听谁先
    // 注册谁先跑，只有从祖先捕获才能保证「选中历史项时那一下 Enter 是我们的」。
    toolbar.addEventListener(
      "keydown",
      function (event) {
        if (event.target !== addressInput(root)) { return; }
        var menu = addressMenu(root);
        var open = !!menu && !menu.hidden;
        if (event.key === "Escape") {
          if (!open) { return; }
          event.preventDefault();
          event.stopPropagation();
          closeAddressMenu(root);
          return;
        }
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
          event.preventDefault();
          event.stopPropagation();
          if (!open) {
            var draft = addressInput(root);
            openAddressMenu(
              panel,
              root,
              root.__starshipAddrTyped && draft ? draft.value : "",
            );
            addrMenuIndex = event.key === "ArrowDown" ? 0 : -1;
            return;
          }
          var rows = menu.querySelectorAll(".starship-addr__row");
          if (!rows.length) { return; }
          addrMenuIndex += event.key === "ArrowDown" ? 1 : -1;
          if (addrMenuIndex < 0) { addrMenuIndex = rows.length - 1; }
          if (addrMenuIndex >= rows.length) { addrMenuIndex = 0; }
          syncActiveAddressRow(menu);
          return;
        }
        if (event.key !== "Enter" || !open || addrMenuIndex < 0) { return; }
        var rows = menu.querySelectorAll(".starship-addr__row");
        var row = rows[addrMenuIndex];
        if (!row || !row.__starshipUrl) { return; }
        event.preventDefault();
        event.stopPropagation();
        navigateFromAddress(panel, root, row.__starshipUrl);
      },
      true,
    );
    return true;
  }
  // ── 下载面板 ───────────────────────────────────────────────────────────────
  // 数据源是壳层 state 里的 `downloads`（`add_DownloadStarting` 收全三条事件后
  // 归并成的账本）。面板不自己去问 WebView2：那里只有「某个标签的某次下载」，
  // 没有跨标签的一张表，而用户关心的是「我刚下的那几个文件去哪了」。
  var dlMenuRoot = null;
  window.__starshipDownloadLog = window.__starshipDownloadLog || [];
  function noteDownload(line) {
    window.__starshipDownloadLog.push(String(line));
    if (window.__starshipDownloadLog.length > 50) { window.__starshipDownloadLog.shift(); }
  }
  function downloadsList() {
    var state = window.__OPENCLAW_NATIVE_BROWSER__;
    var list = state && state.downloads;
    return Object.prototype.toString.call(list) === "[object Array]" ? list : [];
  }
  function formatBytes(value) {
    var bytes = Number(value);
    // 负数在壳层那里是「这次没报 Content-Length」，不是「零字节」。
    if (!isFinite(bytes) || bytes < 0) { return ""; }
    if (bytes < 1024) { return bytes + " B"; }
    var units = ["KB", "MB", "GB", "TB"];
    var size = bytes / 1024;
    var unit = 0;
    while (size >= 1024 && unit < units.length - 1) {
      size = size / 1024;
      unit += 1;
    }
    return (size >= 10 ? size.toFixed(0) : size.toFixed(1)) + " " + units[unit];
  }
  // 中断原因由壳层翻成人话再送上来；这里只把常客写成中文，其余原样显示，免得
  // 前端再欠一张和 COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON 逐条对齐的对照表。
  function downloadReasonText(reason) {
    var table = {
      user_canceled: "\u5df2\u53d6\u6d88",
      user_shutdown: "\u5e94\u7528\u9000\u51fa",
      network_failed: "\u7f51\u7edc\u4e2d\u65ad",
      network_timeout: "\u8fde\u63a5\u8d85\u65f6",
      network_disconnected: "\u7f51\u7edc\u65ad\u5f00",
      server_failed: "\u670d\u52a1\u5668\u62d2\u7edd",
      server_bad_status: "\u670d\u52a1\u5668\u72b6\u6001\u5f02\u5e38",
      server_unauthorized: "\u670d\u52a1\u5668\u672a\u6388\u6743",
      server_forbidden: "\u670d\u52a1\u5668\u62d2\u7edd\u8bbf\u95ee",
      server_not_found: "\u6587\u4ef6\u4e0d\u5b58\u5728",
      server_range_not_satisfiable: "\u670d\u52a1\u5668\u4e0d\u652f\u6301\u7eed\u4f20",
      file_failed: "\u5199\u5165\u78c1\u76d8\u5931\u8d25",
      file_access_denied: "\u78c1\u76d8\u6743\u9650\u4e0d\u8db3",
      file_no_space: "\u78c1\u76d8\u7a7a\u95f4\u4e0d\u8db3",
      file_name_too_long: "\u6587\u4ef6\u540d\u8fc7\u957f",
      file_too_large: "\u6587\u4ef6\u8fc7\u5927",
      file_malformed: "\u6587\u4ef6\u683c\u5f0f\u5f02\u5e38",
      file_security_check_failed: "\u5b89\u5168\u68c0\u67e5\u672a\u901a\u8fc7",
      file_blocked_by_policy: "\u88ab\u7b56\u7565\u963b\u6b62",
    };
    var key = String(reason || "");
    if (Object.prototype.hasOwnProperty.call(table, key)) { return table[key]; }
    return key && key !== "none" ? key : "";
  }
  function downloadMeta(entry) {
    var host = hostOf(entry.url || "");
    var prefix = host ? host + " \u00b7 " : "";
    var state = String(entry.state || "");
    var received = formatBytes(entry.received);
    var total = formatBytes(entry.total);
    if (state === "completed") {
      return prefix + (received || total) + (received || total ? " \u00b7 " : "") +
        "\u5df2\u5b8c\u6210";
    }
    if (state === "interrupted") {
      var reason = downloadReasonText(entry.reason);
      return prefix + "\u5df2\u4e2d\u65ad" + (reason ? "\uff1a" + reason : "");
    }
    if (Number(entry.total) > 0) {
      return prefix + received + " / " + total;
    }
    return prefix + (received ? received + " \u00b7 " : "") + "\u4e0b\u8f7d\u4e2d";
  }
  function downloadRow(entry) {
    var row = document.createElement("button");
    row.type = "button";
    row.className = "starship-dl__row";
    row.setAttribute("role", "option");
    var name = document.createElement("span");
    name.className = "starship-dl__name";
    name.textContent =
      entry.filename || shortAddress(entry.url || "") || "\u4e0b\u8f7d\u6587\u4ef6";
    var meta = document.createElement("span");
    meta.className = "starship-dl__meta";
    meta.textContent = downloadMeta(entry);
    row.appendChild(name);
    row.appendChild(meta);
    if (String(entry.state || "") === "in_progress") {
      var bar = document.createElement("span");
      bar.className = "starship-dl__bar";
      var fill = document.createElement("i");
      var total = Number(entry.total);
      if (total > 0) {
        var ratio = (Number(entry.received) / total) * 100;
        fill.style.width = Math.max(2, Math.min(100, ratio)) + "%";
      } else {
        bar.setAttribute("data-unknown", "1");
      }
      bar.appendChild(fill);
      row.appendChild(bar);
    }
    row.__starshipDownload = entry;
    return row;
  }
  function downloadsMenu(root) {
    return root ? root.querySelector(".starship-dl__menu") : null;
  }
  // 贴着 ⋮ 那颗按钮弹：右对齐到按钮，宽度取 Codex 那份列表的宽度，边缘收一收。
  // 下沿放不下就往上翻，别把列表顶出窗口。
  function placeDownloadsMenu(root, menu) {
    var anchor = root.querySelector(".starship-extras__toggle");
    if (!anchor) { return false; }
    var rect = anchor.getBoundingClientRect();
    var width = Math.min(372, Math.max(window.innerWidth - 16, 240));
    var left = rect.right - width;
    if (left + width > window.innerWidth - 8) { left = window.innerWidth - width - 8; }
    if (left < 8) { left = 8; }
    menu.style.left = Math.round(left) + "px";
    menu.style.width = width + "px";
    menu.style.top = Math.round(rect.bottom + 4) + "px";
    var height = menu.getBoundingClientRect().height;
    if (rect.bottom + 4 + height > window.innerHeight - 8) {
      menu.style.top = Math.max(8, Math.round(rect.top - 4 - height)) + "px";
    }
    return true;
  }
  function panelOfRoot(root) {
    var host = root && root.host;
    if (!host || typeof host.closest !== "function") { return null; }
    return host.closest(PANEL_SELECTOR) || host;
  }
  function fillDownloads(panel, menu, root) {
    var entries = downloadsList();
    while (menu.firstChild) { menu.removeChild(menu.firstChild); }
    var head = document.createElement("div");
    head.className = "starship-dl__head";
    var title = document.createElement("span");
    title.textContent = "\u4e0b\u8f7d";
    head.appendChild(title);
    var actions = document.createElement("span");
    actions.className = "starship-dl__actions";
    var folder = document.createElement("button");
    folder.type = "button";
    folder.className = "starship-dl__action";
    folder.textContent = "\u6253\u5f00\u6587\u4ef6\u5939";
    folder.addEventListener("click", function (event) {
      event.preventDefault();
      event.stopPropagation();
      folder.disabled = true;
      var done = function () { folder.disabled = false; };
      actOnPanel(panel, "downloads", { open: true }).then(done, done);
    });
    actions.appendChild(folder);
    if (entries.length) {
      var clear = document.createElement("button");
      clear.type = "button";
      clear.className = "starship-dl__action";
      clear.textContent = "\u6e05\u7a7a\u8bb0\u5f55";
      clear.addEventListener("click", function (event) {
        event.preventDefault();
        event.stopPropagation();
        // 清的是壳层的账本，不是磁盘上的文件：用户按「清空记录」不想连文件一起没。
        postMessage({ type: "downloads", clear: true }).then(
          function () { fillDownloads(panel, menu, root); },
          function () {},
        );
      });
      actions.appendChild(clear);
    }
    head.appendChild(actions);
    menu.appendChild(head);
    if (!entries.length) {
      var empty = document.createElement("div");
      empty.className = "starship-dl__empty";
      empty.textContent = "\u8fd8\u6ca1\u6709\u4e0b\u8f7d\u8bb0\u5f55";
      menu.appendChild(empty);
      return;
    }
    for (var index = 0; index < entries.length; index += 1) {
      if (!entries[index]) { continue; }
      menu.appendChild(downloadRow(entries[index]));
    }
  }
  function closeDownloadsMenu(root) {
    var ownerRoot = root || dlMenuRoot;
    if (!ownerRoot) { return; }
    var menu = downloadsMenu(ownerRoot);
    dlMenuRoot = null;
    if (!menu || menu.hidden) { return; }
    menu.hidden = true;
    noteDownload("closed");
  }
  function openDownloadsMenu(panel, root) {
    var menu = downloadsMenu(root);
    if (!menu) { return; }
    dlMenuRoot = root;
    if (menu.hidden) {
      menu.hidden = false;
      fillDownloads(panel, menu, root);
      if (!placeDownloadsMenu(root, menu)) {
        closeDownloadsMenu(root);
        return;
      }
      noteDownload("opened rows=" + downloadsList().length);
    }
  }
  // 点外面关掉。和地址栏下拉一样挂 document：官方面板是 Lit 渲染的，挂在面板内部
  // 节点上的监听会随重渲染一起消失。
  function installDownloadsDismiss() {
    if (document.__starshipDownloadDismiss) { return; }
    document.__starshipDownloadDismiss = true;
    document.addEventListener(
      "pointerdown",
      function (event) {
        var path = typeof event.composedPath === "function" ? event.composedPath() : [];
        for (var index = 0; index < path.length; index += 1) {
          var node = path[index];
          if (!node || typeof node.closest !== "function") { continue; }
          // ⋮ 菜单里那一项也在名单上：点它打开下载面板时，那一下 pointerdown 不该
          // 被当成「点外面」，否则面板刚开就关。
          if (node.closest(".starship-dl__menu, .starship-extras__menu, .starship-extras__toggle")) {
            return;
          }
        }
        closeDownloadsMenu(null);
      },
      true,
    );
    window.addEventListener("resize", function () {
      if (!dlMenuRoot) { return; }
      var menu = downloadsMenu(dlMenuRoot);
      if (!menu || menu.hidden) { dlMenuRoot = null; return; }
      placeDownloadsMenu(dlMenuRoot, menu);
    });
    // 进度是壳层推上来的（state.downloads），一份文件在下的时候每次进度帧都改这一份
    // 状态。面板开着就跟着重画，用户不用关掉再打开才看到 100%。
    window.addEventListener("openclaw:native-browser-state", function () {
      if (!dlMenuRoot) { return; }
      var root = dlMenuRoot;
      var menu = downloadsMenu(root);
      if (!menu || menu.hidden) { return; }
      fillDownloads(panelOfRoot(root), menu, root);
      placeDownloadsMenu(root, menu);
    });
  }
  function installDownloadsMenu(panel, root) {
    var menu = downloadsMenu(root);
    if (!menu) {
      menu = document.createElement("div");
      menu.className = "starship-dl__menu";
      menu.setAttribute("role", "listbox");
      // 和地址栏下拉同一个机关：遮挡探测把它当浮层，壳层随即把原生子视图换成
      // 同位置的截图，否则列表下半截会被网页画掉。
      menu.setAttribute("data-starship-overlay", "1");
      menu.hidden = true;
      menu.addEventListener("pointerdown", function (event) { event.preventDefault(); });
      menu.addEventListener("pointerover", function (event) {
        var row = event.target && typeof event.target.closest === "function"
          ? event.target.closest(".starship-dl__row")
          : null;
        if (!row) { return; }
        var rows = menu.querySelectorAll(".starship-dl__row");
        for (var index = 0; index < rows.length; index += 1) {
          if (rows[index] === row) {
            rows[index].setAttribute("data-active", "1");
          } else {
            rows[index].removeAttribute("data-active");
          }
        }
      });
      menu.addEventListener("click", function (event) {
        var row = event.target && typeof event.target.closest === "function"
          ? event.target.closest(".starship-dl__row")
          : null;
        if (!row || !row.__starshipDownload) { return; }
        event.preventDefault();
        event.stopPropagation();
        var entry = row.__starshipDownload;
        // 下完的点开文件，还没下完/中断的在文件夹里定位：两种情况用户想做的事
        // 不一样，合成一个动作只会让人猜。
        var request = String(entry.state || "") === "completed"
          ? { type: "downloads", openFile: entry.path }
          : { type: "downloads", reveal: entry.path };
        postMessage(request).then(
          function (reply) {
            // 壳层把「打开文件」的成败放在 action 里，顶层 ok 说的是这次请求本身
            // 有没有被受理 —— 拿 ok 判成败会把每一次失败都读成成功。
            var action = reply && reply.action;
            if (action && action.ok === false) {
              noteDownload("open failed " + String(action.error || ""));
            }
          },
          function () {},
        );
      });
      root.appendChild(menu);
    }
    installDownloadsDismiss();
    return true;
  }

  watchPanelMutations();
  scanPanels();
  window.setInterval(function () { scheduleParityScan(false); }, 2000);
  // 官方那一行的几何会随窗口宽度变（面板类型胶囊多一个、右侧动作区换一组按钮），
  // 合并时的让位量得跟着重算。重算走同一条防抖通道，不另开计时器。
  window.addEventListener("resize", function () { scheduleParityScan(false); });
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

    /// 站点弹出的 `alert/confirm/prompt/beforeunload` 手柄。
    ///
    /// WebView2 默认会弹自己的模态对话框。子 WebView2 是「挂在 Tauri 窗口上的
    /// 子视图」，那个默认框既不是面板的一部分、又拿不到关闭入口，而渲染进程会
    /// 一直等它 —— 面板上的表现就是「这个标签死了」：`Runtime.evaluate`、
    /// `DOM.*` 全部超时，只有 `Page.*` 还活着。
    ///
    /// 壳层的做法是关掉默认对话框 UI，改由 `ScriptDialogOpening` 取 deferral 接管：
    /// 页面照常阻塞（这是浏览器的正常语义），但解铃的手柄握在壳层手里，可以响应
    /// 驱动的 `dialog` 动作，也能在无人处理时按超时收尾。
    struct PendingDialog {
        args: ICoreWebView2ScriptDialogOpeningEventArgs,
        deferral: ICoreWebView2Deferral,
    }

    // COM 接口不是 `Send`，没法塞进跨线程的 `Command`，只能在 WebView2 所属
    // 线程上存取。`ScriptDialogOpening` 回调和 `with_webview` 闭包都跑在主线程，
    // 所以这份手柄放 thread_local 最贴切；两边都拿不到时也只是退回 CDP 路径，
    // 不会让谁崩掉。
    thread_local! {
        static PENDING_DIALOGS: RefCell<HashMap<String, PendingDialog>> =
            RefCell::new(HashMap::new());
    }

    /// 跨线程可见的「这个标签正被弹窗挡着」摘要。
    ///
    /// 手柄（COM 接口）不能跨线程，只能留在主线程的 `PENDING_DIALOGS`；可动作
    /// 回执是在 worker 线程上拼的，而 `Command::ScriptDialog` 只能排在队尾 ——
    /// 一次点击把页面挡在弹窗里时，那条 CDP 命令会一直等到超时才失败，等它
    /// 失败时「弹窗入账」那条命令还没被 worker 取到。上层于是收到一条裸的
    /// `CDP ... failed`，看不出「页面在等弹窗」，只会傻傻重试同一个动作。
    ///
    /// 所以这里额外留一份能跨线程读的摘要，由 `ScriptDialogOpening` 回调在拿到
    /// deferral 的同一刻写入；动作失败时用它把错误翻译成
    /// `COMPUTER_DIALOG_BLOCKED`。摘要带时间戳，只认「这次动作开始之后冒出来」
    /// 的弹窗，避免拿旧账解释新错。
    #[derive(Clone)]
    struct PendingDialogSummary {
        kind: String,
        message: String,
        default_text: String,
        uri: String,
        raised_at: Instant,
    }

    static PENDING_DIALOG_SUMMARY: Mutex<Vec<(String, PendingDialogSummary)>> =
        Mutex::new(Vec::new());

    fn note_pending_dialog(tab_id: &str, summary: PendingDialogSummary) {
        let Ok(mut entries) = PENDING_DIALOG_SUMMARY.lock() else {
            return;
        };
        entries.retain(|(id, _)| id != tab_id);
        entries.push((tab_id.to_string(), summary));
    }

    fn clear_pending_dialog(tab_id: &str) {
        let Ok(mut entries) = PENDING_DIALOG_SUMMARY.lock() else {
            return;
        };
        entries.retain(|(id, _)| id != tab_id);
    }

    fn pending_dialog_since(tab_id: &str, since: Instant) -> Option<PendingDialogSummary> {
        let entries = PENDING_DIALOG_SUMMARY.lock().ok()?;
        entries
            .iter()
            .find(|(id, summary)| id == tab_id && summary.raised_at >= since)
            .map(|(_, summary)| summary.clone())
    }

    /// 这个标签上此刻还挂着的弹窗摘要（不看时间）。
    ///
    /// 和 `pending_dialog_since` 的唯一区别是不过滤时间：动作回执要回答的问题
    /// 是「页面现在还被弹窗挡着吗」。每条解掉弹窗的路径（驱动 `dialog`、超时
    /// 兜底、标签关闭）都会 `clear_pending_dialog`，所以摘要还在就是还挡着。
    fn pending_dialog(tab_id: &str) -> Option<PendingDialogSummary> {
        let entries = PENDING_DIALOG_SUMMARY.lock().ok()?;
        entries
            .iter()
            .find(|(id, _)| id == tab_id)
            .map(|(_, summary)| summary.clone())
    }

    /// 官方的「页面正等着处理弹窗」回执。
    ///
    /// 驱动拿到这个码就知道该发 `dialog` 动作，而不是重试同一个动作 —— 后者
    /// 在页面被弹窗冻住时永远不会有第二种结果。
    fn dialog_blocked_receipt(
        action: &str,
        kind: &str,
        message: &str,
        default_text: &str,
        uri: &str,
        error: &str,
    ) -> Value {
        json!({
            "ok": false,
            "code": "COMPUTER_DIALOG_BLOCKED",
            "action": action,
            "dialog": {
                "kind": kind,
                "message": message,
                "defaultText": default_text,
                "uri": uri,
            },
            "error": format!(
                "The page is waiting on a {kind} dialog; \
                 resolve it with the \"dialog\" action ({error})"
            ),
        })
    }

    #[derive(Default)]
    pub struct NativeBrowserState {
        bridge: Mutex<Option<Sender<Command>>>,
    }

    enum Command {
        Request { id: String, message: Value },
        TabEvent { tab_id: String, event: TabEvent },
        NewWindow { opener: String, url: String },
        ProcessFailed { tab_id: String, kind: i32 },
        /// 子 WebView 的 WebView2 控制器没能建出来。这是硬失败，必须上屏，
        /// 不能像以前那样只留下一个永远 loading 的空白标签。
        TabUnavailable { tab_id: String, reason: String },
        /// 看门狗：标签建好后迟迟收不到任何加载事件，兜底把 loading 收干净。
        TabWatchdog { tab_id: String },
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
            /// The measured panel had the chrome layer's rail merge applied, so
            /// this rect is the post-merge truth rather than the pre-merge
            /// geometry the dashboard measures while it remounts a pane.
            merged: bool,
        },
        /// 面板主题变了（官方 UI 在 `<html data-theme>` 上表达）。
        ///
        /// 为什么要报给壳层：内嵌网页的 `prefers-color-scheme` 由 WebView2 的
        /// **颜色方案**决定，默认跟着 Windows，而不是跟着星舰自己的主题。用户把
        /// 星舰切成浅色、系统是深色时，面板里的网页会是另一套配色。dashboard 的
        /// 注入层每次主题变化报一声，壳层据此设 `PreferredColorScheme`。
        Theme { theme: String },
        /// 到点做一次整页适配。延迟必须由独立的计时线程回投：在 worker 线程上
        /// 睡觉会把整个命令队列（含用户刚点的那一下）一起堵住。
        FitTabZoom {
            tab_id: String,
            width: f64,
            generation: u64,
            /// 第二遍复测。复测不把缩放摘回 100%，只判断内容是不是比上屏那会儿
            /// 更宽了（图片/脚本晚到的站点会这样）。
            retry: bool,
        },
        /// 站点弹出了对话框，壳层已经把 deferral 攥在手里。只带元数据：手柄本身
        /// 留在主线程的 `PENDING_DIALOGS`。
        ScriptDialog {
            tab_id: String,
            kind: String,
            message: String,
            default_text: String,
            uri: String,
        },
        /// 无人处理时的兜底：超时还没等到驱动动作，就把弹窗按默认语义收掉，
        /// 免得一个 `alert` 让标签永久卡住。
        DialogTimeout { tab_id: String, sequence: u64 },
        /// 下载生命周期的一帧。官方那份面板没有下载 UI，工具菜单里那条
        /// 「下载文件夹」只是打开资源管理器 —— 用户看不到进度、看不到成败、
        /// 也不知道文件落在哪。这里把 WebView2 的下载事件投回 worker 记账，
        /// 面板再照着账本画列表。
        Download { event: DownloadEvent },
    }

    /// WebView2 下载事件的三段：开始 / 进度 / 结局。
    ///
    /// `id` 是壳层自己发的序号（`add_DownloadStarting` 不给下载对象任何可传
    /// 出去的标识），三条命令靠它归并到同一条记录上。
    enum DownloadEvent {
        Started {
            id: u64,
            tab_id: String,
            uri: String,
            path: String,
            total: i64,
        },
        Progress {
            id: u64,
            received: i64,
            total: i64,
        },
        /// 终态。`state`/`reason` 是 `COREWEBVIEW2_DOWNLOAD_STATE` 与
        /// `COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON` 的原始值，翻译交给上层，
        /// 免得壳层硬编码一遍迟早对不上的字符串表。
        ///
        /// 字节数在这里再带一遍：进度事件是抽稀发的（见 `download_progress_due`），
        /// 最后一截很可能落在抽稀窗口里，只有终态这一帧能保证把准确的总数送上屏。
        Finished {
            id: u64,
            state: i32,
            reason: i32,
            path: String,
            received: i64,
            total: i64,
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
        /// 用户在页面里真的动手了（见 `TAB_INIT_SCRIPT` 的「人手优先」探针）。
        /// 合成事件不算：只有 `isTrusted` 的输入才走这条账。
        HumanInput(HumanInput),
    }

    /// 用户最近一次在页面里动手的现场。
    ///
    /// 只记「有人动过手」是不够的：那样智能体就只能等用户彻底停手。记下落点
    /// 和目标之后，壳层才能分清「他点的是左边那个按钮」和「他现在盯着的地方
    /// 正是我要点的」—— 前者互不相干，后者才该让路。
    #[derive(Clone)]
    struct HumanInput {
        at: Instant,
        kind: String,
        /// 事件落点（指针类事件才有；键盘事件的 clientX/clientY 恒为 0）。
        point: Option<(f64, f64)>,
        /// 事件命中的元素（最近的带 `data-starship-ref` 的祖先）。
        reference: Option<String>,
        /// 这一下是往输入框里敲字 —— 键盘冲突的判据，不看坐标。
        typing: bool,
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
        /// 这个标签收到过多少个加载事件。0 表示子 WebView 从未真正存在过 ——
        /// 这是 WebView2 创建失败唯一可靠的判别信号。
        events: u32,
        /// 星舰为「装进面板」自动施加的缩放系数。`None` 表示当前没有自动缩放，
        /// 页面按 100% 渲染（这正是没溢出的站点该有的状态）。
        auto_fit: Option<f64>,
        /// 最近一次做适配判定的面板宽度。上屏几何一变（拖分隔条、窗口最大化），
        /// 为旧宽度算出的缩放就不再合适，要用这个值判断该不该重算。
        fit_width: Option<f64>,
        /// 施加自动缩放时页面所在的 URL。站内翻页不需要重来，跨站才重新量 ——
        /// 否则每点一条新闻都会闪一下字号。
        fit_url: String,
        /// 用户在面板里手动缩放过这个标签（工具栏的 +/-/100%）。此后壳层不再自动
        /// 改它的缩放：用户的选择优先。
        zoom_dirty: bool,
        /// 适配请求的代次。排队中的旧请求在到达时对不上代次就丢弃，所以连续拖动
        /// 分隔条只会跑最后那一次重算。
        fit_generation: u64,
        /// 用户最近一次在这个标签的页面里动手的现场（「人手优先」）。
        ///
        /// `None` = 用户没碰过这个标签。壳层拿它和智能体这一下的落点比，
        /// **撞上同一块地方才让路**，撞不上就照常干（判定见 `human_conflict`）。
        human_input: Option<HumanInput>,
        webview: Webview,
    }

    #[derive(Clone, Copy, PartialEq)]
    struct Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    /// 同一个面板槽位上的两份几何：左右边界要能对上（真实的窗口/分隔条缩放会同时
    /// 改这两项），上屏那份比合并后**低**、且差不到两条横栏（合并只上移一条横栏，
    /// 中间态还夹着一帧动画）。对得上就说明这是「标签行还没抬上去」的量法，而不是
    /// 面板真的换了位置。
    fn same_panel_slot(merged: &Rect, presented: &Rect) -> bool {
        (merged.x - presented.x).abs() <= 8.0
            && (merged.width - presented.width).abs() <= 8.0
            && presented.y > merged.y
            && presented.y - merged.y <= 96.0
            && (merged.height - presented.height).abs() <= 96.0
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
        /// The stage geometry the probe measured. This is the *post-merge* truth:
        /// the dashboard measures its own panel before the chrome layer lifts the
        /// tab row into the rail, so its own presentations can lag a row behind.
        rect: Option<Rect>,
        /// Dashboard chrome is open over the stage, so the native child view has
        /// to stay hidden or it would cover that chrome.
        occluded: bool,
        /// Whether the panel the probe measured had the rail merge applied.
        /// Only a merged measurement may become `Worker::last_merged`, because
        /// the probe also runs while the dashboard remounts a pane and measures
        /// the stage a row lower.
        merged: bool,
        at: Instant,
    }

    /// How long a probe stays usable as the authority for "which pane owns the
    /// native view". The injected script heartbeats once a second, so anything
    /// older than a few beats means the dashboard document is gone (navigation,
    /// unload) and the guard has to fall back to accepting every presentation.
    const PROBE_FRESHNESS: Duration = Duration::from_millis(3500);

    /// How long the last probe geometry keeps outranking the dashboard's own
    /// presentation after the probe stops reporting a visible panel. The probe
    /// goes quiet for a moment whenever the dashboard remounts a pane (it
    /// measures the stage while it is still 0x0), and the dashboard's own rect
    /// at that instant is the *pre-merge* one - a row lower. Handing that rect
    /// to the child view is exactly the "panel jumps down a row on click" flash,
    /// so the last trustworthy geometry holds through the gap.
    const PROBE_GRACE: Duration = Duration::from_millis(2500);

    /// 遮挡替身能复用多久。菜单开合之间画面基本没变，反复截图既慢又会闪，
    /// 所以一个遮挡回合里只截一帧，短时间内重开菜单也接着用那一帧。
    const STANDIN_FRESH: Duration = Duration::from_millis(2000);

    /// 「宁可贴旧图也不留白板」的窗口。
    ///
    /// 超出 [`STANDIN_FRESH`] 但还在这个窗口里的替身，只在**拍不到新帧**时才顶上
    /// 去（遮挡让位必须有画面垫底，见 `ensure_standin`）。再旧就不贴了：那张图
    /// 可能已经对不上页面内容，宁可让原生视图留在原位（见 `held_winners`）——
    /// 内容是真的，只是会压住菜单。
    const STANDIN_STALE: Duration = Duration::from_secs(8);

    /// 地址栏历史保留多少条。只按 URL 去重，够铺满下拉列表远超出可见行数，
    /// 同时给落盘的 `browser-history.json` 定了个上限。
    const HISTORY_LIMIT: usize = 200;

    /// 一次 `history` 查询里最多现抓几个站点的图标。
    ///
    /// 图标只能问「此刻开着那个站点的标签页」要，一次抓取要走一趟 CDP 往返。
    /// 下拉列表首屏只显示十几条，所以按需补前若干条就够；再往后条目退化成
    /// 首字母色块，不能为了补图标把一次查询拖成几百毫秒。
    const FAVICON_HYDRATE_LIMIT: usize = 12;

    /// 下载列表保留多少条（最新的在最前）。
    ///
    /// 官方那份面板根本就没有下载 UI，所以这条列表是星舰自己加的：它是用户
    /// 「刚才下的那个文件去哪了」的唯一答案。旧条目只在超出这个上限时被挤掉，
    /// 不做时间清理 —— 一次会话里下一个大文件可能是半小时前的事。
    const DOWNLOAD_LIMIT: usize = 50;

    /// 历史条目和图标缓存都按「站点」聚合，key 用 origin。
    fn origin_key(url: &str) -> String {
        let Ok(parsed) = Url::parse(url) else {
            return url.to_string();
        };
        let Some(host) = parsed.host_str() else {
            return url.to_string();
        };
        match parsed.port() {
            Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
            None => format!("{}://{host}", parsed.scheme()),
        }
    }

    /// 权限种类 → 配置和日志里用的名字。
    ///
    /// 刻意不用 WebView2 的枚举名（`COREWEBVIEW2_PERMISSION_KIND_CAMERA`）：
    /// 要手写进环境变量的东西得是人话。编号来自 `COREWEBVIEW2_PERMISSION_KIND`。
    fn permission_kind_name(kind: i32) -> &'static str {
        match kind {
            1 => "microphone",
            2 => "camera",
            3 => "geolocation",
            4 => "notifications",
            5 => "sensors",
            6 => "clipboard-read",
            7 => "automatic-downloads",
            8 => "file-read-write",
            9 => "autoplay",
            10 => "local-fonts",
            11 => "midi-sysex",
            12 => "window-management",
            _ => "unknown",
        }
    }

    /// 权限放行清单的开关。逗号分隔，单个项有两种写法：
    ///
    /// * `camera` —— 任意站点，放行这一类；
    /// * `example.com=camera|microphone` —— 只放行指定站点上的这几类。
    ///
    /// 为什么不是一个总开关：`autoplay`（静音自动播放）和 `camera` 完全不是
    /// 一回事。一律拒绝会把良性的那一半也打掉；一律放行则等于把设备交出去。
    const PERMISSION_ALLOW_ENV: &str = "STARSHIP_BROWSER_ALLOW_PERMISSIONS";

    /// 这一次权限请求该不该放行。没配、配错、站点对不上，一律是「不放行」——
    /// 放行必须是显式写出来的。
    fn permission_allowed(origin: &str, kind: &str) -> bool {
        let Ok(raw) = std::env::var(PERMISSION_ALLOW_ENV) else {
            return false;
        };
        // 站点按主机名比：`https://www.example.com:8443` → `www.example.com`。
        let host = origin
            .split("://")
            .nth(1)
            .unwrap_or(origin)
            .split(['/', ':'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        for entry in raw.split(',') {
            let entry = entry.trim().to_ascii_lowercase();
            if entry.is_empty() {
                continue;
            }
            match entry.split_once('=') {
                Some((site, kinds)) => {
                    let site = site.trim().trim_start_matches('.');
                    // 后缀匹配而不是 `contains`：`e.com` 不该悄悄放行
                    // `example.com`。写成 `example.com` 仍然覆盖 `www.example.com`。
                    let site_matches = !host.is_empty()
                        && (host == site || host.ends_with(&format!(".{site}")));
                    if !site_matches {
                        continue;
                    }
                    if kinds
                        .split('|')
                        .map(str::trim)
                        .any(|name| name == kind || name == "*")
                    {
                        return true;
                    }
                }
                None => {
                    if entry == kind || entry == "*" {
                        return true;
                    }
                }
            }
        }
        false
    }

    thread_local! {
        /// 已经记过日志的 `(站点, 权限)`。
        ///
        /// 被拒的站点常常每隔几秒重试一次（`getUserMedia` 失败后自动重试），
        /// 逐条落盘会把壳层日志淹掉 —— 那本日志是排查问题用的。
        static PERMISSION_LOGGED: RefCell<HashSet<(String, String)>> =
            RefCell::new(HashSet::new());
    }

    fn permission_should_log(origin: &str, kind: &str) -> bool {
        PERMISSION_LOGGED.with(|set| set.borrow_mut().insert((origin.to_string(), kind.to_string())))
    }

    /// 下载序号。WebView2 的下载回调之间没有任何可传递的标识，归并只能靠壳层
    /// 自己发号。
    static DOWNLOAD_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    /// 进度事件的抽稀闸门。
    ///
    /// `BytesReceivedChanged` 是按数据块触发的，一个大文件能把它打成几百上千次。
    /// 每一次都投一条命令回去，就等于用下载进度把 worker 的队列灌满 —— 用户的
    /// 点击排在这些进度后面，面板会明显发木。所以这里按「距上次上屏的间隔」放行，
    /// 而终态由 `StateChanged` 保证按实数送达，不会因为抽稀丢掉结尾那一截。
    const DOWNLOAD_PROGRESS_GAP: Duration = Duration::from_millis(200);

    thread_local! {
        /// 下载 id -> 上一次放行进度的时间。
        static DOWNLOAD_PROGRESS_AT: RefCell<HashMap<u64, Instant>> =
            RefCell::new(HashMap::new());
    }

    /// 这一帧进度该不该上屏。首次一定放行（用户要立刻看到「开始了」）。
    fn download_progress_due(id: u64, now: Instant) -> bool {
        DOWNLOAD_PROGRESS_AT.with(|map| {
            let mut map = map.borrow_mut();
            match map.get(&id) {
                Some(previous) if now.duration_since(*previous) < DOWNLOAD_PROGRESS_GAP => false,
                _ => {
                    map.insert(id, now);
                    true
                }
            }
        })
    }

    /// 下载结束后把闸门记录删掉，长会话里下载多了不至于把这张表撑起来。
    fn download_progress_forget(id: u64) {
        DOWNLOAD_PROGRESS_AT.with(|map| {
            map.borrow_mut().remove(&id);
        });
    }

    /// `COREWEBVIEW2_DOWNLOAD_STATE` → 上屏用的名字。
    fn download_state_name(state: i32) -> &'static str {
        if state == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED.0 {
            "completed"
        } else if state == COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED.0 {
            "interrupted"
        } else {
            "in_progress"
        }
    }

    /// 下载中断原因 → 人话。编号来自 `COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON`。
    ///
    /// 和 `permission_kind_name` 同一个理由：要上屏的东西得是人话，但也不值得把
    /// 三十个枚举常量全 import 进来。没列出的编号退化成 `"unknown"`，界面照常显示
    /// 「下载中断」，不至于因为运行库多了一个枚举值就整行画不出来。
    fn download_reason_name(reason: i32) -> &'static str {
        match reason {
            // 0 是 `_NONE`：下载完成时它就是 0，所以这里回空串而不是一个「原因」。
            0 => "",
            1 => "file-failed",
            2 => "file-access-denied",
            3 => "file-no-space",
            4 => "file-name-too-long",
            5 => "file-too-large",
            6 => "file-malicious",
            7 => "file-transient-error",
            8 => "file-blocked-by-policy",
            9 => "file-security-check-failed",
            10 => "file-too-short",
            11 => "file-hash-mismatch",
            12 => "network-failed",
            13 => "network-timeout",
            14 => "network-disconnected",
            15 => "network-server-down",
            16 => "network-invalid-request",
            17 => "server-failed",
            18 => "server-no-range",
            19 => "server-bad-content",
            20 => "server-unauthorized",
            21 => "server-certificate-problem",
            22 => "server-forbidden",
            23 => "server-unexpected-response",
            24 => "server-content-length-mismatch",
            25 => "server-cross-origin-redirect",
            26 => "user-canceled",
            27 => "user-shutdown",
            28 => "user-paused",
            29 => "download-process-crashed",
            _ => "unknown",
        }
    }

    /// 落盘路径 → 文件名。列表上那一行显示的就是它。
    ///
    /// 从路径取而不是从 URL 或 `Content-Disposition` 取：用户要认的是磁盘上那个
    /// 名字（重名时会被加 `(1)`），URI 里的名字和真正落盘的名字经常不是一回事。
    fn file_name_of(path: &str) -> String {
        let trimmed = path.trim_end_matches(['\\', '/']);
        match trimmed.rsplit(['\\', '/']).next() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => trimmed.to_string(),
        }
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis() as i64)
            .unwrap_or(0)
    }

    /// 把面板主题翻译成 WebView2 的颜色方案。
    ///
    /// 设的是 **profile**（`ICoreWebView2Profile::PreferredColorScheme`），不是单个
    /// 视图：同一份 user data folder 下所有 WebView2 共用一个 profile，因此一次设置
    /// 同时作用于已经在的标签和之后新建的标签。
    ///
    /// 只认 `light` / `dark`；别的值（含 `None`）一律不动，保持 WebView2 默认的
    /// 「跟随系统」，绝不用一个猜出来的主题去覆盖用户系统里的选择。
    fn apply_preferred_color_scheme(webview: &Webview, theme: Option<&str>) {
        let scheme = match theme {
            Some("dark") => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_DARK,
            Some("light") => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_LIGHT,
            _ => return,
        };
        let _ = webview.with_webview(move |platform| {
            let _ = crate::crash_log::guard("browser.preferred-color-scheme", move || {
                let Ok(core) = (unsafe { platform.controller().CoreWebView2() }) else {
                    return;
                };
                let Ok(profile_owner) = core.cast::<ICoreWebView2_13>() else {
                    return;
                };
                let Ok(profile) = (unsafe { profile_owner.Profile() }) else {
                    return;
                };
                let _: Result<(), _> =
                    unsafe { profile.SetPreferredColorScheme(scheme) };
            });
        });
    }

    /// 原生子视图让位期间贴在面板原位的那一帧画面。
    ///
    /// 子 WebView2 是独立的 OS 子窗口，永远画在网页之上，所以菜单一开就必须把它
    /// 藏起来，否则菜单下半截被网页盖住。但「藏起来」不等于「抽走」：直接 hide
    /// 会在面板位置留下一块纯空白（用户报的「点工具就白屏」）。先把当前画面截成
    /// 一帧贴回原位，操作菜单时看到的就是「菜单浮在网页上」。
    struct StandIn {
        tab_id: String,
        image: String,
        captured_at: Instant,
        /// 是否已经贴到面板上，避免每个心跳重复投递几百 KB 的图。
        posted: bool,
    }

    struct Worker {
        app: AppHandle,
        revision: u64,
        tabs: Vec<Tab>,
        scopes: HashMap<String, Presentation>,
        order: u64,
        seen: HashSet<String>,
        fallback: Option<FallbackPresentation>,
        /// When `fallback` was captured, so `PROBE_GRACE` can bound how long it
        /// outlives the probe that produced it.
        fallback_at: Option<Instant>,
        /// When the probe last reported that no panel is on screen. A hidden
        /// probe is the user closing the panel; the shell must not keep showing
        /// the child view then.
        probe_hidden_at: Option<Instant>,
        /// Last geometry the probe measured on a panel that was already merged.
        /// Outlives the panel being closed on purpose: the dashboard re-announces
        /// a remounted (or freshly opened) panel with its *pre-merge* rect, and
        /// this is what that announcement is corrected against; see
        /// `merged_geometry`.
        last_merged: Option<(Rect, Instant)>,
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
        /// Occlusion stand-in currently shown over the stage (see [`StandIn`]).
        standin: Option<StandIn>,
        active_tab_id: Option<String>,
        session_restored: bool,
        /// Dashboard document that last handshaked, so the tab list is replayed
        /// once per navigation instead of once per process.
        document_id: Option<String>,
        /// 地址栏下拉用的访问记录，最新的在最前。和 `browser-session.json` 同目录
        /// 落盘（`browser-history.json`），所以关掉客户端再打开还在。
        history: Vec<Value>,
        /// 站点图标缓存：origin -> data URL。
        ///
        /// 存 data URL 而不是原始网址是必须的：dashboard 的 CSP 会把外链图片拦
        /// 掉，只有内联的 data URL 才画得出来。
        favicons: HashMap<String, String>,
        /// 壳层正在接管的站点弹窗，按标签记（一个页面同一时刻只会有一个）。
        /// 值里的 `sequence` 是给超时兜底用的：新弹窗一进来，旧计时器就作废。
        dialogs: HashMap<String, DialogState>,
        dialog_sequence: u64,
        /// 壳层希望面板切过去的标签（站点自己 `window.open` 的那个地址已经
        /// 开着时，我们不再复制一份，而是把已有标签请到前台）。面板消费一次
        /// 就够，所以它只是「最后一条请求」，不是持续状态。
        focus_tab_id: Option<String>,
        /// 下载账本，最新的在最前。见 [`DownloadRecord`]。
        downloads: Vec<DownloadRecord>,
        /// 面板当前主题（`light` / `dark`），由 dashboard 的注入层报上来。
        /// 新标签一出生就按它设颜色方案；没报到之前是 `None`（跟着系统走）。
        theme: Option<String>,
    }

    /// 一次下载在壳层里的记账。
    ///
    /// `received`/`total` 用 `i64` 而不是 `u64`：WebView2 的
    /// `TotalBytesToReceive` 在拿不到 Content-Length 时是 `-1`，那是「未知」，
    /// 不是「零字节」；前端要靠这个负号决定是画进度条还是画走马灯。
    #[derive(Clone)]
    struct DownloadRecord {
        id: u64,
        tab_id: String,
        uri: String,
        path: String,
        received: i64,
        total: i64,
        /// `COREWEBVIEW2_DOWNLOAD_STATE` 的原始值：0 进行中 / 1 已中断 / 2 已完成。
        state: i32,
        /// `COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON` 的原始值，只在中断时有意义。
        reason: i32,
        started_at: i64,
    }

    /// 待决弹窗里可以跨线程传的那一半（元数据）。手柄在 `PENDING_DIALOGS`。
    #[derive(Clone)]
    struct DialogState {
        kind: String,
        message: String,
        default_text: String,
        uri: String,
        sequence: u64,
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

    /// 新建标签后等多久还没等到任何加载事件，就认定这个子 WebView 没建出来。
    /// 正常页面哪怕再慢，`NavigationStarting` 也是立刻发的，所以这个值只用于兜底，
    /// 不怕给宽：宁可晚两秒报错，也不要把慢站点误判成故障。
    const TAB_CREATE_TIMEOUT: Duration = Duration::from_secs(8);

    /// 整页自动适配的阈值。
    ///
    /// 为什么需要它：中文门户/资讯站几乎都按**固定桌面宽度**排版（财联社首页
    /// `.w-1200`），而星舰面板比 Codex 的内嵌浏览器窄，直接按 100% 渲染就是
    /// 「右侧被切掉 + 底部一条横向滚动条」。Codex 面板里的同一页面是全的，因为它
    /// 把整页缩到刚好装下再渲染。这里用同一策略，但只在溢出量合理时动手：
    /// 溢出太少不值得缩（反而让字变糊），溢出太多通常说明本来就不是桌面版
    /// （移动版页面 / 长图），缩下去会小到没法读。
    const FIT_MIN_OVERFLOW: f64 = 1.04;
    const FIT_MAX_OVERFLOW: f64 = 2.0;
    /// 自动缩放的下限：再小就不是「适配」而是「弄坏」了。
    const FIT_MIN_ZOOM: f64 = 0.5;
    /// 导航完成后留给页面排版的时间。内容比 HTML 晚到的站点由复测兜底。
    const FIT_DELAY: Duration = Duration::from_millis(350);
    /// 面板刚上屏（或刚换宽度）到几何稳定下来的时间。
    const FIT_PRESENT_DELAY: Duration = Duration::from_millis(250);
    /// 第二遍复测间隔。复测只允许再量一次，避免反复缩放。
    const FIT_RETRY_DELAY: Duration = Duration::from_millis(900);
    /// 面板宽度变化超过这个比例，为旧宽度算好的缩放就不再适用。
    const FIT_WIDTH_TOLERANCE: f64 = 0.04;
    /// 人手碰过的**那一块地方**，多久之内智能体不去碰。
    ///
    /// 只对撞在同一块地方的动作用得着：用户点了左边的按钮，智能体点右边
    /// 照走不误。1.2 秒够盖住一次点击的余波，又短到用户只是想看一眼、
    /// 马上又让智能体接着干。
    const HUMAN_QUIET_MS: u64 = 1_200;
    /// 用户正在写的那个输入框，多久之内智能体不往里打字。
    ///
    /// 比 `HUMAN_QUIET_MS` 长得多是故意的：写字中间停一下想下一句是常事，
    /// 这时候智能体把字打进同一个框，用户看到的是「我的光标里冒出别人的话」。
    /// 目标不是那个框就不走这条 —— 该并行的时候要并行。
    const HUMAN_EDIT_MS: u64 = 4_000;
    /// 判定「同一块地方」的半径（页面 CSS 像素）。
    ///
    /// 取的是手指/鼠标的落点精度量级：同一个按钮上的两次点击必然落在里面，
    /// 隔壁按钮则在外面。
    const HUMAN_TOUCH_RADIUS_PX: f64 = 48.0;
    /// 智能体动作前给页面盖的窗口长度。CDP 派发的输入事件到达渲染进程是异步
    /// 的，窗口必须比动作本身活得久，否则动作尾巴上的事件会被记成人手。
    const HUMAN_ARM_MS: u64 = 4_000;
    /// 动作结束后留下的余量，理由同上：回执回来时事件可能还在路上。窗口太长
    /// 会把用户真实的操作吃掉，所以这里是「动作时长 + 700ms」，不是常驻静音。
    const HUMAN_ARM_SLACK_MS: u64 = 700;
    /// 把缩放摘回 100% 之后，留给 WebView2 重新排版的毫秒数。测量必须站在
    /// 已知基准上，否则量到的是「当前缩放下的视口」而不是页面的真实排版宽度。
    const FIT_MEASURE_SETTLE: Duration = Duration::from_millis(140);
    /// 小于这个差值的缩放变化不值得再设一次（避免在阈值附近来回抖）。
    const FIT_ZOOM_EPSILON: f64 = 0.005;

    /// 站点弹窗交给驱动处置的时限。到点还没人认领就按默认语义收掉。
    ///
    /// 为什么必须有：接管之后的弹窗是「壳层攥着 deferral」，如果就这么放着，
    /// 页面会一直等下去 —— 那就把原来「WebView2 自己弹框卡死」换成了「壳层
    /// 攥着不放卡死」，等于没修。超时兜底是这条路的最后一道闸。
    const DIALOG_AUTO_DISMISS: Duration = Duration::from_secs(10);

    /// 解一个已接管的弹窗最多等多久。
    ///
    /// 这一步不含任何页面往返（纯本地的一次 `Accept` + `Complete`），所以给得
    /// 比 CDP 短：真卡住时宁可报错，也别把 worker 的命令队列堵上十秒。
    const DIALOG_RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

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
                        // `ensure-browser` 是壳层自己按节奏打的健康检查，正常态
                        // 一次对话能攒上百条，日志里只留真正来自界面的消息。
                        let quiet = matches!(&command, Some(Command::ShellProbe { .. }))
                            || matches!(
                                &command,
                                Some(Command::Request { message, .. })
                                    if message.get("type").and_then(Value::as_str)
                                        == Some("ensure-browser")
                            );
                        if !quiet {
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
        // 主题报告走这条：它是 dashboard 的注入层单发的信号，没有 `id`，
        // 也不需要回包，所以必须在 `__starship` 那条请求路径之前先认掉。
        if let Some(theme) = value.get("__starshipTheme").and_then(Value::as_str) {
            if matches!(theme, "light" | "dark") {
                return Some(Command::Theme {
                    theme: theme.to_string(),
                });
            }
        }
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
            let merged = probe
                .get("merged")
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
                merged,
            });
        }
        let id = value.get("id")?.as_str()?.to_string();
        let message = value.get("message")?.clone();
        Some(Command::Request { id, message })
    }

    /// 子 WebView 的消息通道只认一种报文：`TAB_INIT_SCRIPT` 里「人手优先」
    /// 探针写的那一条（`{"__starshipTab":true,"kind":"pointerdown"}`）。
    ///
    /// `WebMessageAsJson` 给的是 JSON 编码后的值 —— `postMessage(JSON.stringify(x))`
    /// 收到的是一段**带引号的字符串**，所以要按 JSON 解一次、再对字符串解一次，
    /// 和 `parse_inbound` 处理 `__starship` 那外层是同一个套路。认不出的一律
    /// 返回 `None`：页面脚本也能往这个通道里塞东西，别让它们拼出别的事件。
    fn parse_tab_human_input(text: &str) -> Option<HumanInput> {
        let outer: Value = serde_json::from_str(text).ok()?;
        let value = match outer {
            Value::String(inner) => serde_json::from_str(&inner).ok()?,
            other => other,
        };
        if value.get("__starshipTab") != Some(&Value::Bool(true)) {
            return None;
        }
        let kind = value.get("kind")?.as_str()?.trim().to_string();
        if kind.is_empty() || kind.len() > 32 {
            return None;
        }
        // 落点和目标的字段都是可选的：老探针（只报 kind）照样记账，只是
        // 那一笔没有现场可比，壳层就只能按「动过手」这一档来判。
        let point = match (
            value.get("x").and_then(Value::as_f64),
            value.get("y").and_then(Value::as_f64),
        ) {
            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Some((x, y)),
            _ => None,
        };
        let reference = value
            .get("ref")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|reference| !reference.is_empty() && reference.len() <= 64)
            .map(str::to_string);
        Some(HumanInput {
            at: Instant::now(),
            kind,
            point,
            reference,
            typing: value.get("typing") == Some(&Value::Bool(true)),
        })
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
            fallback_at: None,
            probe_hidden_at: None,
            last_merged: None,
            applied: Vec::new(),
            probe: None,
            occluded: false,
            standin: None,
            active_tab_id: None,
            session_restored: false,
            document_id: None,
            history: Vec::new(),
            favicons: HashMap::new(),
            dialogs: HashMap::new(),
            dialog_sequence: 0,
            focus_tab_id: None,
            downloads: Vec::new(),
            theme: None,
        };
        worker.restore_history();
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
                    // Bringing the attach-only endpoint up can take a second or
                    // two on a cold machine. The worker also keeps the native
                    // child views aligned with the panel, so this one answers
                    // from its own thread instead of stalling the queue.
                    if message.get("type").and_then(Value::as_str) == Some("ensure-browser") {
                        self.spawn_ensure_browser(id);
                        return;
                    }
                    let reply = self.handle_request(&message);
                    self.reply(&id, reply);
                }
                Command::Theme { theme } => {
                    if self.theme.as_deref() == Some(theme.as_str()) {
                        return;
                    }
                    // `PreferredColorScheme` 设在 profile 上：同一个 user data folder
                    // 下的所有 WebView2 共用一份 profile，所以设一次既覆盖已经在的
                    // 标签，也覆盖之后新建的标签（新建那一下还会再补一次，见 `open_tab`）。
                    let ids: Vec<String> = self.tabs.iter().map(|tab| tab.id.clone()).collect();
                    let mut applied = 0usize;
                    for id in &ids {
                        if let Some(webview) = self.webview(id) {
                            apply_preferred_color_scheme(&webview, Some(theme.as_str()));
                            applied += 1;
                        }
                    }
                    bridge_log(&format!("theme {theme}: applied to {applied} tab(s)"));
                    self.theme = Some(theme);
                }
                Command::TabEvent { tab_id, event } => {
                    let url_changed = matches!(event, TabEvent::Url(_));
                    let title_changed = matches!(event, TabEvent::Title(_));
                    let settled = matches!(event, TabEvent::Loading(false));
                    if self.apply_event(&tab_id, event) {
                        self.push_state();
                        // 地址栏历史跟着导航走：URL 一落地就先记一条（此时标题还是
                        // 空的），标题和加载完成时再各补一次，同一条 URL 原地合并。
                        if url_changed || title_changed || settled {
                            self.record_history(&tab_id);
                        }
                        if url_changed {
                            self.persist_session();
                        }
                        if settled {
                            // 导航收尾：页面不再长宽了，这时候量出来的溢出才作数。
                            if let Some(width) = self.panel_width(&tab_id) {
                                self.schedule_fit(&tab_id, width, FIT_DELAY);
                            }
                        }
                    }
                }
                Command::FitTabZoom {
                    tab_id,
                    width,
                    generation,
                    retry,
                } => self.fit_tab_zoom(&tab_id, width, generation, retry),
                Command::NewWindow { opener, url } => {
                    if valid_url(&url) {
                        // 站点自己开的新窗口（`window.open` / `target=_blank`
                        // 的修饰键路径）。同一个地址已经在面板里开着的时候
                        // 不再复制一个标签：登录回跳、站内推荐位这类流程会反
                        // 复点名同一个 URL，堆两份只会让用户对着两个一样的
                        // 页面发呆。把已经开着的那一个请到前台就够了。
                        let existing = self
                            .tabs
                            .iter()
                            .find(|tab| tab.url == url)
                            .map(|tab| tab.id.clone());
                        match existing {
                            Some(tab_id) => {
                                self.focus_tab_id = Some(tab_id.clone());
                                self.note_active_tab(&tab_id);
                                self.push_state();
                            }
                            None => {
                                let _ = self.open_tab(None, &url, "native", Some(opener));
                            }
                        }
                    }
                }
                Command::ProcessFailed { tab_id, kind } => {
                    self.recover_tab(&tab_id, kind);
                }
                Command::TabUnavailable { tab_id, reason } => {
                    self.fail_tab(&tab_id, &reason);
                }
                Command::TabWatchdog { tab_id } => {
                    self.timeout_tab(&tab_id);
                }
                Command::ScriptDialog {
                    tab_id,
                    kind,
                    message,
                    default_text,
                    uri,
                } => self.open_script_dialog(tab_id, kind, message, default_text, uri),
                Command::DialogTimeout { tab_id, sequence } => {
                    self.timeout_script_dialog(&tab_id, sequence);
                }
                Command::Download { event } => {
                    // 账本一变就推一次状态：下载列表挂在同一条 `__starshipState`
                    // 通道上，面板不用另开拉取，进度也就跟着标签状态一起上屏。
                    if self.apply_download(event) {
                        self.push_state();
                    }
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
                    merged,
                    ..
                } => {
                    let now = Instant::now();
                    let snapshot = ProbeSnapshot {
                        visible,
                        tab_id: tab_id.clone(),
                        scope: scope.clone(),
                        rect,
                        occluded,
                        merged,
                        at: now,
                    };
                    // 只有「合并之后量到的」几何才有资格当兜底基准。官方重挂面板那
                    // 一瞬探针量到的还是第二行的几何，收下它反而会让下一帧的校正
                    // 反过来把正确的合并几何顶掉。
                    if snapshot.visible && snapshot.merged {
                        if let Some(rect) = snapshot.rect {
                            self.last_merged = Some((rect, now));
                        }
                    }
                    self.probe = Some(snapshot);
                    let occlusion_changed = self.occluded != occluded;
                    if occlusion_changed {
                        bridge_log(if occluded {
                            "shell occluded: native browser view stepped aside"
                        } else {
                            "shell occlusion cleared: native browser view restored"
                        });
                        self.occluded = occluded;
                    }
                    if visible {
                        self.probe_hidden_at = None;
                    } else {
                        self.probe_hidden_at = Some(now);
                    }
                    // 官方侧还在 present 着这块面板（面板没关），而探针只是静默了
                    // 一瞬（pane 重挂载时 `.bp-stage` 会短暂量成 0×0）。这一瞬官方
                    // present 带的是**合并之前**的几何，标签行还在第二行；照它摆原生
                    // 视图，用户看到的就是「点一下先跳到第二行」。所以这段空窗里保住
                    // 上一次的合并后几何，由 PROBE_GRACE 兜住上限。
                    let dashboard_live = self
                        .scopes
                        .values()
                        .any(|scope| scope.visible && scope.tab_id.is_some());
                    let holding = !visible
                        && dashboard_live
                        && self.fallback.is_some()
                        && self
                            .fallback_at
                            .map(|at| at.elapsed() < PROBE_GRACE)
                            .unwrap_or(false);
                    let next = match (visible, tab_id, rect) {
                        (true, Some(tab_id), Some(rect)) => {
                            Some(FallbackPresentation { tab_id, rect })
                        }
                        (false, ..) if holding => self.fallback.clone(),
                        _ => None,
                    };
                    let captured = visible && next.is_some();
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
                        if captured {
                            self.fallback_at = Some(now);
                        }
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

        /// 智能体这一下会不会撞上用户的手。
        ///
        /// 返回 `None` = 可以继续。这是**共存**规则，不是「用户先做完，智能体
        /// 才能动」—— 那样用户一边看着页面，智能体就一边停着，等于没人干活。
        /// 判据是现场，不是「有人动过手」：
        ///
        /// * **观察类**（`snapshot`/`screenshot`/`wait`/`Runtime.*`）根本不进
        ///   这道闸：用户在打字的时候，智能体本来就该还能看一眼页面。
        /// * **指针类**（click / hover / move / drag / select）只有落在用户刚
        ///   碰过的那一块才让路 —— 同一个元素，或者 `HUMAN_TOUCH_RADIUS_PX`
        ///   以内、`HUMAN_QUIET_MS` 以内。用户点左边、智能体点右边，照常走。
        /// * **键盘类**（type / key / press）只认键盘冲突，不看坐标：按键事件
        ///   只会落到当前有焦点的那个元素上，智能体一开打字，字就落进用户的
        ///   光标里。目标是用户**正在写的那个框**时窗口更长（`HUMAN_EDIT_MS`）。
        /// * **滚动**和用户的滚动撞在一起一定让路：同一个视口，两次滚动会互相
        ///   顶掉，用户看到的是「页面自己跳了」。
        ///
        /// 让路给的是 `retryAfterMs` 而不是「失败」：上层照着重试就行，不用猜。
        fn human_conflict(
            &self,
            tab_id: &str,
            label: &str,
            writing: bool,
            viewport: bool,
            point: Option<(f64, f64)>,
            reference: Option<&str>,
        ) -> Option<Value> {
            let tab = self.tabs.iter().find(|tab| tab.id == tab_id)?;
            let human = tab.human_input.as_ref()?;
            let elapsed = human.at.elapsed();
            // 键盘冲突不看坐标，所以「撞车」这一档对写字动作恒真；真正决定
            // 要不要让路的是下面那个窗口有多长。
            let collides = writing || viewport || same_target(human, point, reference);
            if !collides {
                return None;
            }
            let editing_target = writing
                && human.typing
                && reference.is_some()
                && human.reference.as_deref() == reference;
            let window = Duration::from_millis(if editing_target {
                HUMAN_EDIT_MS
            } else {
                HUMAN_QUIET_MS
            });
            if elapsed >= window {
                return None;
            }
            let wait_ms = (window - elapsed).as_millis().max(1) as u64;
            let reason = if writing {
                "keyboard"
            } else if viewport {
                "scroll"
            } else {
                "pointer"
            };
            bridge_log(&format!(
                "shell human priority hold tab={tab_id} action={label} reason={reason} \
                 wait_ms={wait_ms}"
            ));
            Some(json!({
                "ok": false,
                "code": "COMPUTER_HUMAN_INPUT",
                "action": label,
                "conflict": reason,
                "retryAfterMs": wait_ms,
                "error": format!(
                    "The user is working on that spot right now; retry in about {wait_ms} ms, \
                     or leave that part of the page to them"
                ),
            }))
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
                    let Some(raw_url) = message.get("url").and_then(Value::as_str) else {
                        return invalid_request();
                    };
                    let Some(url) = sanitize_url(raw_url) else {
                        return invalid_request();
                    };
                    // 地址栏在没有活动标签时也要能当「新建标签」用：用户从历史里选
                    // 一条、或在空面板上按回车，这条请求可能不带 tabId（面板侧确实
                    // 没有可用的 `activeTargetId`），也可能带了一个刚被关掉的旧 id。
                    // 老实现分别回 `invalid_request` / `unknown_tab`，落到界面上就是
                    // 「点了没反应、浏览器不出来」。这两种情况都从 open 起步。
                    let tab_id = self
                        .tab_id(message)
                        .filter(|id| self.webview(id).is_some());
                    let Some(tab_id) = tab_id else {
                        return match self.open_tab(None, raw_url, "web", None) {
                            Ok(tab_id) => json!({ "ok": true, "tabId": tab_id, "opened": true }),
                            Err(error) => json!({ "ok": false, "error": error }),
                        };
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
                "history" => {
                    // 地址栏下拉的数据源。返回最近的若干条，最新的在最前；
                    // 前面的条目顺手把站点图标补成 data URL（见
                    // `FAVICON_HYDRATE_LIMIT`），补不到就让前端退化成色块。
                    let limit = message
                        .get("limit")
                        .and_then(Value::as_u64)
                        .unwrap_or(40)
                        .clamp(1, HISTORY_LIMIT as u64) as usize;
                    let mut entries: Vec<Value> =
                        self.history.iter().take(limit).cloned().collect();
                    let mut hydrated = 0usize;
                    for entry in entries.iter_mut() {
                        let url = entry
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let favicon = entry
                            .get("favicon")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if favicon.is_empty() && hydrated < FAVICON_HYDRATE_LIMIT {
                            hydrated += 1;
                            let resolved = self.favicon_for_url(&url);
                            if !resolved.is_empty() {
                                if let Some(object) = entry.as_object_mut() {
                                    object.insert("favicon".to_string(), json!(resolved));
                                }
                            }
                        }
                    }
                    json!({ "ok": true, "entries": entries })
                }
                "downloads" => {
                    // 下载账本的查询/管理口。面板画的是壳层记账的那一份（`state`
                    // 里一直在推），这里回答「现在有什么」以及「清空 / 打开 / 定位」。
                    // 不回头问 WebView2：那边只有「某个标签的某一次下载」，没有一张
                    // 跨标签的表，而用户问的是「我刚下的文件去哪了」。
                    if message
                        .get("clear")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        // 只清账本，不动磁盘：用户按「清空记录」不是要删自己的文件。
                        self.downloads.clear();
                        self.push_state();
                    }
                    let mut action = Value::Null;
                    if let Some(path) = message.get("openFile").and_then(Value::as_str) {
                        action = match open_download_path(path, false) {
                            Ok(()) => json!({ "ok": true }),
                            Err(error) => json!({ "ok": false, "error": error }),
                        };
                    } else if let Some(path) = message.get("reveal").and_then(Value::as_str) {
                        action = match open_download_path(path, true) {
                            Ok(()) => json!({ "ok": true }),
                            Err(error) => json!({ "ok": false, "error": error }),
                        };
                    }
                    let limit = message
                        .get("limit")
                        .and_then(Value::as_u64)
                        .unwrap_or(DOWNLOAD_LIMIT as u64)
                        .clamp(1, DOWNLOAD_LIMIT as u64) as usize;
                    let mut entries = self.downloads_payload();
                    entries.truncate(limit);
                    json!({
                        "ok": true,
                        "entries": entries,
                        // 落盘目录由壳层定（`add_DownloadStarting` 写 ResultFilePath），
                        // 面板要显示「打开文件夹」就得知道是哪，别让前端自己拼路径。
                        "directory": download_directory(None)
                            .map(|path| path.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        "action": action,
                    })
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
                    match elements(&webview, message) {
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
                    // 落点得在 `cdp_params` 把 `params` 吃掉之前读出来：判定比
                    // 编码早，两次读的是同一份参数。
                    let pointer = finite_point(&params);
                    let encoded = cdp_params(params);
                    if encoded.len() > 100_000 {
                        return invalid_request();
                    }
                    let Some(webview) = self.webview(&tab_id) else {
                        return unknown_tab();
                    };
                    // 「人手优先」在 `dispatch` 这条通道上一样成立：它也能往页面里
                    // 灌输入（`Input.dispatchMouseEvent` / `insertText` 之类），用户
                    // 正用着页面时照样会抢。观察类命令（`Runtime.*` / `DOM.*`）不
                    // 受影响 —— 用户打字的时候，智能体本来就该还能看一眼页面。
                    let injects = dispatch_injects_input(method);
                    if injects {
                        if let Some(receipt) = self.human_conflict(
                            &tab_id,
                            method,
                            dispatch_reads_keyboard(method),
                            dispatch_moves_viewport(method),
                            pointer,
                            None,
                        ) {
                            return receipt;
                        }
                        arm_agent_input(&webview, now_ms() + HUMAN_ARM_MS as i64);
                    }
                    let raw = call_cdp(&webview, method, &encoded);
                    if injects {
                        arm_agent_input(&webview, now_ms() + HUMAN_ARM_SLACK_MS as i64);
                    }
                    match raw {
                        Ok(raw) => {
                            let result =
                                serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw));
                            json!({ "ok": true, "method": method, "result": result })
                        }
                        // 失败原样带出来（方法名 / HRESULT / 应答体已由 `call_cdp` 拼好），
                        // 不再压成一句「CDP xxx failed」。
                        Err(error) => json!({ "ok": false, "method": method, "error": error }),
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
                    // 官方 Computer Use 的两条契约错误码在这里对齐：动作名不认识
                    // 是契约不匹配，观察过期是陈旧观测。上层的处置完全不同——
                    // 前者要改参数，后者只要重新看一眼。
                    if !known_act_action(action) {
                        return json!({
                            "ok": false,
                            "code": "COMPUTER_CONTRACT_MISMATCH",
                            "action": action,
                            "error": format!("Unsupported action: {action}"),
                        });
                    }
                    if let Some(observation) = message.get("observationId").and_then(Value::as_str)
                    {
                        let current = observation_token(&webview);
                        if current.as_deref() != Some(observation) {
                            return json!({
                                "ok": false,
                                "code": "COMPUTER_STALE_OBSERVATION",
                                "observationId": current,
                                "error": "The page moved on since that observation; \
                                          read the panel state again before acting",
                            });
                        }
                    }
                    // 「人手优先」：用户刚刚在这个页面上动过手，这一下就先让给他。
                    // 判定放在观测校验之后、动作之前 —— 观测过期是上层的错，
                    // 用户在用则是这个页面的现状，两种拒绝的处置完全不同。
                    if act_injects_input(action) {
                        if let Some(receipt) = self.human_conflict(
                            &tab_id,
                            action,
                            act_reads_keyboard(action),
                            act_moves_viewport(action),
                            human_target_point(message),
                            act_reference(message),
                        ) {
                            return receipt;
                        }
                    }
                    // 记下动作起点：下面的错误翻译只认「这一下之后冒出来」的
                    // 弹窗，免得拿上一轮没清干净的账解释这次的失败。
                    let act_started_at = Instant::now();
                    // 动作前后都要盖章，原因见 `arm_agent_input`：前面那一次挡住
                    // 动作自己发出来的可信事件，后面那一次收成一小截尾巴。
                    if act_injects_input(action) {
                        arm_agent_input(&webview, now_ms() + HUMAN_ARM_MS as i64);
                    }
                    let outcome = perform_act(&webview, &tab_id, action, message);
                    if act_injects_input(action) {
                        // 尾巴只留 `HUMAN_ARM_SLACK_MS`，**不**沿用动作前那一段：
                        // 动作已经跑完，剩下的事件都是余波，长尾巴只会把用户
                        // 接下来的操作一起吃掉 —— 那正是「智能体和我抢」的成因。
                        arm_agent_input(&webview, now_ms() + HUMAN_ARM_SLACK_MS as i64);
                    }
                    // 用户在面板里手动缩放过（工具栏的 +/-/100%），此后壳层不再
                    // 自动改这个标签的缩放：人的选择优先于自动适配。
                    // 唯一的例外是「适配宽度」——那一下要的正是把控制权交还
                    // 给自动适配，所以它清掉 dirty 并重量一次面板宽度。
                    let fit_requested = action == "zoom"
                        && message.get("direction").and_then(Value::as_str) == Some("fit");
                    if outcome.is_ok() && action == "zoom" {
                        let width = self.panel_width(&tab_id);
                        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                            tab.zoom_dirty = !fit_requested;
                            if fit_requested {
                                // 手动缩放改掉了排版基准，按旧宽度记下的结论
                                // 不再作数，置空才不会被同宽跳过。
                                tab.fit_width = None;
                            }
                        }
                        if fit_requested {
                            if let Some(width) = width {
                                self.schedule_fit(&tab_id, width, FIT_MEASURE_SETTLE);
                            }
                        }
                    }
                    // 弹窗这一下处理完就撤账：超时兜底线程醒来时序号已经作废，
                    // 不会再对着同一个标签补发一次动作。
                    if action == "dialog" && outcome.is_ok() {
                        self.forget_dialog(&tab_id);
                    }
                    match outcome {
                        Ok(detail) => {
                            // 页面被站点弹窗挡着时，动作的「效果」根本没法确认：
                            // 真实输入栈那三条 `Input.*` 命令照样能完成（它们排在
                            // 弹窗冻结之前），紧接着的观测却读不到页面 —— CDP 求值
                            // 要等满超时才回来。于是回执会拼成「confirmed +
                            // page:null」这种假成功，上层拿着它继续往下走，看到的
                            // 是一个已经冻住的页面。这里先把这层遮羞布撤掉：有弹窗
                            // 压着就报官方的 `COMPUTER_DIALOG_BLOCKED`，顺带省掉
                            // 那次注定超时的观测（十秒）。
                            if let Some((dialog_kind, dialog_message, dialog_default, dialog_uri)) =
                                self.blocked_dialog(&tab_id)
                            {
                                return dialog_blocked_receipt(
                                    action,
                                    &dialog_kind,
                                    &dialog_message,
                                    &dialog_default,
                                    &dialog_uri,
                                    &format!(
                                        "the page is frozen on a {dialog_kind} dialog, \
                                         so the effect could not be observed"
                                    ),
                                );
                            }
                            // 动作可视化：弹窗那一支已经提前 return 过了，能走到
                            // 这里说明页面是活的 —— 在页面上画一笔（光标/涟漪/
                            // 拖拽轨迹/滚动指示/元素描边）。它不参与回执，投递
                            // 失败也只是少画一笔。
                            visualize_act(&webview, action, message, &detail);
                            // 回执里带上当前观测序号，上层接着动作继续用同一个
                            // 序号即可，不用为了拿它再多看一眼页面。
                            let page = page_state(&webview);
                            if page.is_null() {
                                // 没有弹窗背锅却读不到页面：这多半是标签正在收尾
                                // 或渲染进程出问题，留一条账给下次排查看。
                                bridge_log(&format!(
                                    "act observation unavailable tab={tab_id} action={action}"
                                ));
                            }
                            let observation =
                                page.get("observationId").cloned().unwrap_or(Value::Null);
                            json!({
                                "ok": true,
                                "effect": "confirmed",
                                "action": action,
                                "detail": detail,
                                "observationId": observation,
                                "page": page,
                            })
                        }
                        Err(error) => {
                            // 这个标签正被站点弹窗挡着：动作多半已经落到了页面上，
                            // 只是回执被弹窗截住了（页面一停，CDP 的完成回调也不
                            // 来）。如实报成「等你处理弹窗」，附上弹窗内容，上层
                            // 就知道该发 `dialog` 动作，而不是傻乎乎重试同一动作。
                            //
                            // 两条来源缺一不可：worker 已经把弹窗入账时用
                            // `self.dialogs`；还没轮到它入账（命令排在这次动作后
                            // 面）时用跨线程摘要兜底 —— 后者正是「点一下把页面挡
                            // 住」的常规时序。
                            let dialog = self
                                .dialogs
                                .get(&tab_id)
                                .map(|dialog| {
                                    (
                                        dialog.kind.clone(),
                                        dialog.message.clone(),
                                        dialog.default_text.clone(),
                                        dialog.uri.clone(),
                                    )
                                })
                                .or_else(|| {
                                    pending_dialog_since(&tab_id, act_started_at).map(|summary| {
                                        (
                                            summary.kind,
                                            summary.message,
                                            summary.default_text,
                                            summary.uri,
                                        )
                                    })
                                });
                            if let Some((kind, message, default_text, uri)) = dialog {
                                return dialog_blocked_receipt(
                                    action,
                                    &kind,
                                    &message,
                                    &default_text,
                                    &uri,
                                    &error,
                                );
                            }
                            json!({ "ok": false, "error": error })
                        }
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

        /// Official `present` - and the probe's own first measurement right after the
        /// dashboard remounts a pane - are taken while the panel's tab row is still
        /// its own second row, so they sit a whole rail height lower than the merged
        /// panel. The probe only reports the merged geometry a few hundred
        /// milliseconds later, and handing the pre-merge rect to the child view for
        /// that long is exactly the "panel drops a row on click, then snaps back"
        /// flash. Correct it against the last geometry the probe measured on a
        /// merged panel: same slot, one row lower - that is the intermediate state,
        /// not a move.
        fn merged_geometry(&self, rect: Rect) -> Rect {
            let Some((merged, at)) = self.last_merged else {
                return rect;
            };
            if at.elapsed() >= PROBE_GRACE {
                return rect;
            }
            if same_panel_slot(&merged, &rect) {
                merged
            } else {
                rect
            }
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
                // The probe measures the stage *after* the chrome layer lifted the
                // tab row into the official rail, so its rect is the only geometry
                // that describes the merged panel. The dashboard's own presents are
                // measured before that merge and can sit a whole row lower.
                if let (Some(tab_id), Some(rect)) = (probe.tab_id.as_deref(), probe.rect) {
                    winners.insert(tab_id.to_string(), (u64::MAX, self.merged_geometry(rect)));
                }
                return winners;
            }
            let mut dashboard: HashMap<String, (u64, Rect)> = HashMap::new();
            for scope in self.scopes.values() {
                if !scope.visible {
                    continue;
                }
                let (Some(tab_id), Some(rect)) = (scope.tab_id.as_deref(), scope.rect) else {
                    continue;
                };
                match dashboard.get(tab_id) {
                    Some((order, _)) if *order >= scope.order => {}
                    _ => {
                        dashboard.insert(tab_id.to_string(), (scope.order, rect));
                    }
                }
            }
            if dashboard.is_empty() {
                // Nothing on the official side presents a tab right now. This is
                // the input-ownership gap the fallback exists for (the panel is
                // visibly open but the dashboard handed input to the assistant
                // dock) - but a probe that reported the panel gone means the user
                // closed it, and then the child view must not linger on screen.
                if self.probe_hidden_at.is_none() {
                    if let Some(fallback) = &self.fallback {
                        winners.insert(
                            fallback.tab_id.clone(),
                            (u64::MAX, self.merged_geometry(fallback.rect)),
                        );
                    }
                }
                return winners;
            }
            if let Some(fallback) = &self.fallback {
                let held = self
                    .fallback_at
                    .map(|at| at.elapsed() < PROBE_GRACE)
                    .unwrap_or(false);
                if held && dashboard.contains_key(&fallback.tab_id) {
                    // The probe went quiet a moment ago while the dashboard still
                    // presents this tab: that is a remount hiccup, not a closed
                    // panel. The dashboard's rect there is the pre-merge one, so
                    // keep the merged geometry instead of letting the child view
                    // drop a row and snap back.
                    winners.insert(
                        fallback.tab_id.clone(),
                        (u64::MAX, self.merged_geometry(fallback.rect)),
                    );
                    return winners;
                }
            }
            // The dashboard is the authority here only because the probe is not
            // live; its geometry can still be the pre-merge one (panel remount,
            // first tab in a fresh panel). Correct those before they reach the
            // child view.
            for (_, (_, rect)) in dashboard.iter_mut() {
                *rect = self.merged_geometry(*rect);
            }
            dashboard
        }

        /// 遮挡这一帧拿不到替身时的几何：把「最后已知的位置」还给这个标签，让原生
        /// 视图留在屏上，而不是让它消失成一块白板。
        ///
        /// 这是 [`Self::presentation_winners`] 在遮挡态下唯一的替代品。官方探针报
        /// 遮挡时它**刻意**返回空表（把舞台让给菜单），但「让位」只有在壳层手里有
        /// 一张能贴回去的画面时才成立；没有画面还照样让位，面板就是白的。留下一
        /// 帧视图不只为看得见，它同时把下一帧的截图条件（视图在屏上）恢复了。
        fn held_winners(&self, tab_id: Option<&str>) -> HashMap<String, (u64, Rect)> {
            let mut winners: HashMap<String, (u64, Rect)> = HashMap::new();
            let target = tab_id
                .map(str::to_string)
                .or_else(|| self.fallback.as_ref().map(|fallback| fallback.tab_id.clone()));
            let Some(target) = target else {
                return winners;
            };
            if let Some((_, Some(rect))) = self
                .applied
                .iter()
                .find(|(id, rect)| id == &target && rect.is_some())
            {
                winners.insert(target, (u64::MAX, *rect));
                return winners;
            }
            // 还没上过屏（刚开客户端、刚建标签）：用兜底几何，位置可能差一点点，
            // 但这一帧本来就是异常帧，下一帧就会被替身上位取代。
            if let Some(fallback) = &self.fallback {
                if fallback.tab_id == target {
                    winners.insert(target, (u64::MAX, self.merged_geometry(fallback.rect)));
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

        /// 问 dashboard 现在是什么主题。
        ///
        /// 注入层的「主题变了」是推模型，会漏首条：脚本在 document 建立时就跑，而
        /// 壳层的 dashboard 消息处理器是**之后**才挂上去的（见 `attach_dashboard_handler`
        /// 的 attach 重试）。所以新建标签时按需拉一次，保证「一出生就跟面板同色」；
        /// 之后主题再变，推模型负责。
        fn dashboard_theme(&self) -> Option<String> {
            let webview = self.app.get_webview("main")?;
            let raw = execute_script(
                &webview,
                "(() => { const root = document.documentElement; if (!root) { return null; } \
                 const theme = root.getAttribute('data-theme') || ''; \
                 if (theme === 'light' || theme === 'dark') { return theme; } \
                 return root.classList.contains('wa-dark') ? 'dark' : 'light'; })()"
                    .to_string(),
            )?;
            let value: Value = serde_json::from_str(&raw).ok()?;
            match value.as_str() {
                Some("light") => Some("light".to_string()),
                Some("dark") => Some("dark".to_string()),
                _ => None,
            }
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
            let mut builder = WebviewBuilder::new(label, WebviewUrl::External(target))
                .focused(false)
                // 页面内行为策略（`target="_blank"` 点击改写）必须落在**每个标签自己的
                // 文档**里：它管的是页面里的链接点击，和 dashboard 那份面板骨架注入层
                // 不是一回事。放这里还有个好处——改策略只需要导航一次，不用重编译。
                .initialization_script(crate::native_browser_tab_script())
                .on_new_window(move |url, _features| {
                    let _ = popup_sender.send(Command::NewWindow {
                        opener: popup_tab.clone(),
                        url: url.to_string(),
                    });
                    NewWindowResponse::Deny
                });
            // 必须和主 WebView 用同一份 WebView2 附加参数。同一个 user data folder
            // 下 `CoreWebView2EnvironmentOptions` 不一致时，WebView2 会直接拒绝创建，
            // 而 Tauri 把这次失败只写进日志、`add_child` 仍返回 Ok，面板上就留下一个
            // 永远 loading 的空白标签。参数取自 main.rs 里的同源入口。
            if let Some(args) = crate::webview_debug_browser_args() {
                builder = builder.additional_browser_args(args.as_str());
            }
            let webview = window
                .add_child(
                    builder,
                    LogicalPosition::new(-32000.0, -32000.0),
                    LogicalSize::new(1.0, 1.0),
                )
                .map_err(|error| format!("Could not create native browser tab: {error}"))?;
            // 主题：新标签一出生就按面板当前主题渲染，别等用户去切一次主题。
            // （profile 级设置本就会带给新视图，这一发是「此刻还没有任何标签时
            // 就报过主题」那种顺序的兜底。）
            if self.theme.is_none() {
                self.theme = self.dashboard_theme();
            }
            apply_preferred_color_scheme(&webview, self.theme.as_deref());
            let sender = self.sender()?;
            attach_tab_events(&webview, &id, sender);
            let watchdog = self.sender()?;
            let watchdog_tab = id.clone();
            // `add_child` 返回 Ok 不代表子 WebView 真的建出来了（见上面的说明）。
            // 所以每个新标签都挂一个看门狗：到点仍没收到任何加载事件，就说明这个
            // 子视图根本不存在，必须把 loading 收干净并上屏原因，而不是让它一直转。
            thread::spawn(move || {
                thread::sleep(TAB_CREATE_TIMEOUT);
                let _ = watchdog.send(Command::TabWatchdog {
                    tab_id: watchdog_tab,
                });
            });
            self.tabs.push(Tab {
                id: id.clone(),
                url,
                title: String::new(),
                loading: true,
                can_go_back: false,
                can_go_forward: false,
                opened_by,
                opener_tab_id: opener,
                events: 0,
                auto_fit: None,
                fit_width: None,
                fit_url: String::new(),
                zoom_dirty: false,
                fit_generation: 0,
                human_input: None,
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
            // 待决弹窗必须先解掉再关：deferral 不 Complete，页面会一直等，
            // WebView2 那边也就没法干净地回收这个标签。
            if self.dialogs.remove(tab_id).is_some() {
                let _ = complete_native_dialog(&tab.webview, tab_id, false, None);
            }
            // 弹窗刚冒出来、`Command::ScriptDialog` 还排在队尾时 `self.dialogs`
            // 里是空的，可跨线程摘要已经落了；标签一关它就没有意义了。
            clear_pending_dialog(tab_id);
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

        /// 站点弹窗进了壳层：记账、通知面板、挂超时兜底。
        ///
        /// 真正的对话框手柄留在主线程的 `PENDING_DIALOGS` 里（COM 接口不是
        /// `Send`，不能跨线程搬），这里只留能跨线程传的元数据。
        fn open_script_dialog(
            &mut self,
            tab_id: String,
            kind: String,
            message: String,
            default_text: String,
            uri: String,
        ) {
            // 标签已经没了（页面收尾时补发的回调）：不再记账。
            if !self.tabs.iter().any(|tab| tab.id == tab_id) {
                clear_pending_dialog(&tab_id);
                return;
            }
            self.dialog_sequence = self.dialog_sequence.wrapping_add(1);
            let sequence = self.dialog_sequence;
            bridge_log(&format!(
                "tab dialog tab={tab_id} kind={kind} chars={} url={uri}",
                message.chars().count()
            ));
            self.dialogs.insert(
                tab_id.clone(),
                DialogState {
                    kind,
                    message,
                    default_text,
                    uri,
                    sequence,
                },
            );
            self.push_state();
            self.schedule_dialog_dismiss(&tab_id, sequence);
        }

        /// 超时兜底：驱动一直没来认领，就按默认语义收掉。
        ///
        /// `beforeunload` 之外一律按「取消」收：自动按「确定」是替用户答应站点，
        /// 自动取消最坏只是这一下没生效。
        fn timeout_script_dialog(&mut self, tab_id: &str, sequence: u64) {
            let Some(state) = self.dialogs.get(tab_id) else {
                return;
            };
            if state.sequence != sequence {
                return;
            }
            let kind = state.kind.clone();
            let mode = if kind == "beforeunload" {
                "accept"
            } else {
                "dismiss"
            };
            bridge_log(&format!(
                "tab dialog timeout tab={tab_id} kind={kind} mode={mode}"
            ));
            self.dispatch_dialog(tab_id, mode);
        }

        /// 用和驱动完全同一条路径（`perform_act("dialog")`）收掉弹窗，然后清账。
        fn dispatch_dialog(&mut self, tab_id: &str, mode: &str) {
            let Some(webview) = self.webview(tab_id) else {
                self.forget_dialog(tab_id);
                return;
            };
            let message = json!({ "tabId": tab_id, "mode": mode });
            if let Err(error) = perform_act(&webview, tab_id, "dialog", &message) {
                bridge_log(&format!(
                    "tab dialog resolve failed tab={tab_id} mode={mode}: {error}"
                ));
            }
            self.forget_dialog(tab_id);
        }

        /// 这个标签的弹窗处理完了（或页面已经不在了）。
        fn forget_dialog(&mut self, tab_id: &str) {
            clear_pending_dialog(tab_id);
            if self.dialogs.remove(tab_id).is_some() {
                self.push_state();
            }
        }

        /// 这个标签上还挡着页面的站点弹窗：worker 已经入账的优先（带序号、能
        /// 直接处置），还没轮到入账的用跨线程摘要兜底。
        fn blocked_dialog(&self, tab_id: &str) -> Option<(String, String, String, String)> {
            self.dialogs
                .get(tab_id)
                .map(|dialog| {
                    (
                        dialog.kind.clone(),
                        dialog.message.clone(),
                        dialog.default_text.clone(),
                        dialog.uri.clone(),
                    )
                })
                .or_else(|| {
                    pending_dialog(tab_id).map(|summary| {
                        (
                            summary.kind,
                            summary.message,
                            summary.default_text,
                            summary.uri,
                        )
                    })
                })
        }

        /// 超时兜底要独立线程回投：在 worker 线程上睡觉会把整个命令队列
        /// （含用户刚点的那一下）一起堵住。
        fn schedule_dialog_dismiss(&mut self, tab_id: &str, sequence: u64) {
            let Ok(sender) = self.sender() else {
                return;
            };
            let tab_id = tab_id.to_string();
            let _ = thread::Builder::new()
                .name("starship-native-browser-dialog".to_string())
                .spawn(move || {
                    thread::sleep(DIALOG_AUTO_DISMISS);
                    let _ = sender.send(Command::DialogTimeout { tab_id, sequence });
                });
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
            tab.events = tab.events.saturating_add(1);
            match event {
                TabEvent::Url(url) => {
                    if let Some(url) = sanitize_url(&url) {
                        if !same_site(&tab.url, &url) {
                            // 跨站了：上一站的适配缩放和它记住的宽度都不再成立，
                            // 下一次适配会从 100% 重新量。用户上一站手动调过的缩放
                            // 也是「那一站的」，新站重新自动适配。
                            tab.auto_fit = None;
                            tab.fit_width = None;
                            tab.fit_url.clear();
                            tab.zoom_dirty = false;
                        }
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
                TabEvent::HumanInput(input) => {
                    // 只记账，**不**推状态：人手滚一下轮子就是一串事件，每次都
                    // 推一遍面板状态，前端会被无谓地重绘（而且这一帧用户正在
                    // 看页面，重绘只会让他觉得卡）。
                    bridge_log(&format!(
                        "shell human input tab={tab_id} kind={} ref={} point={} typing={}",
                        input.kind,
                        input.reference.as_deref().unwrap_or("-"),
                        match input.point {
                            Some((x, y)) => format!("{x:.0},{y:.0}"),
                            None => "-".to_string(),
                        },
                        input.typing,
                    ));
                    tab.human_input = Some(input);
                    return false;
                }
            }
            true
        }

        /// 子 WebView 建失败时，把原因放到标签上让用户看得见。
        ///
        /// 在这之前 `add_child` 返回 Ok 就被当成成功，于是 WebView2 建失败只会
        /// 表现为"正在加载页面"永远转下去，日志里连一行线索都没有。
        fn fail_tab(&mut self, tab_id: &str, reason: &str) {
            let changed = match self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                Some(tab) => {
                    tab.loading = false;
                    tab.title = format!("页面无法打开：{reason}");
                    true
                }
                None => false,
            };
            bridge_log(&format!("shell tab unavailable tab={tab_id} reason={reason}"));
            if changed {
                self.push_state();
            }
        }

        /// 看门狗：这个标签一个加载事件都没收到过，说明子 WebView 从未真正存在。
        ///
        /// 只认 `events == 0`，所以慢站点不会被误报 —— 再慢的页面，
        /// `NavigationStarting` 也是立刻发出的。
        fn timeout_tab(&mut self, tab_id: &str) {
            let stalled = match self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                Some(tab) if tab.loading && tab.events == 0 => {
                    tab.loading = false;
                    tab.title = "页面无法打开：内嵌浏览器未创建（子 WebView 缺失）".to_string();
                    true
                }
                _ => false,
            };
            if stalled {
                let url = self
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .map(|tab| tab.url.clone())
                    .unwrap_or_default();
                bridge_log(&format!("shell tab watchdog fired tab={tab_id} url={url}"));
                self.push_state();
            }
        }

        fn apply_presentations(&mut self) {
            // 遮挡判断要和几何判断用同一份探针：菜单开着的这一帧，正确的做法不是
            // 「让原生视图消失」，而是「先把它现在的画面留下来」。截图必须在
            // `hide()` 之前完成——视图一旦藏起来就拍不到自己了。
            let probe = self.live_probe().cloned();
            let occluded = probe.as_ref().map(|probe| probe.occluded).unwrap_or(false);
            let occluded_tab = probe.as_ref().and_then(|probe| probe.tab_id.clone());
            let mut standin: Option<String> = None;
            if occluded {
                if let Some(tab_id) = occluded_tab.as_deref() {
                    standin = self.ensure_standin(tab_id);
                }
            }
            // 让位的前提是手里有一张能被贴回去的画面。一张都拿不到的时候（这一刻
            // 视图已经不在屏上、缓存里又没有可用的帧），宁可让原生视图留在原位：
            // 它会压住菜单，但「压住」是看得见、而且下一帧就能自愈的——视图回到
            // 屏上，截图随之成功，替身补上之后它再让位。直接 hide 留下的是用户报
            // 的那个「点开工具一片空白」，而且只要探针一直报遮挡就一直是白的。
            let hold_open = occluded && standin.is_none();
            let winners = if hold_open {
                self.held_winners(occluded_tab.as_deref())
            } else {
                self.presentation_winners()
            };
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
            // 面板宽度变了（拖分隔条、窗口最大化、侧栏开合）——为旧宽度算好的
            // 缩放就不再合适。这里只排队，真正的测量与施加在 `fit_tab_zoom` 里。
            let refits: Vec<(String, f64)> = applied
                .iter()
                .filter_map(|(tab_id, rect)| {
                    let rect = (*rect)?;
                    let tab = self.tabs.iter().find(|tab| tab.id == *tab_id)?;
                    if tab.zoom_dirty {
                        return None;
                    }
                    match tab.fit_width {
                        Some(last) => {
                            let drift = (rect.width - last).abs() / last.max(1.0);
                            if drift > FIT_WIDTH_TOLERANCE {
                                Some((tab_id.clone(), rect.width))
                            } else {
                                None
                            }
                        }
                        // 还没适配过（刚建出来 / 刚重建 / 刚上屏）→ 补一次。
                        None => Some((tab_id.clone(), rect.width)),
                    }
                })
                .collect();
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
            for (tab_id, width) in refits {
                self.schedule_fit(&tab_id, width, FIT_PRESENT_DELAY);
            }
            // 贴替身 / 撤替身都排在几何之后：原生视图先让位（或先回位），图片再换，
            // 中间那一帧不会出现「两边都没有」的空窗。
            match standin {
                Some(image) => self.post_standin(image),
                None if !occluded => self.clear_standin(),
                None => {}
            }
        }

        /// 遮挡替身要用的那一帧。
        ///
        /// 同一个标签在一个遮挡回合里只截一次：`hide()` 之后子视图拍不到自己，
        /// 而且菜单开合的间隔通常只有几百毫秒，复用上一帧既快又不会闪。
        fn ensure_standin(&mut self, tab_id: &str) -> Option<String> {
            if let Some(standin) = &self.standin {
                if standin.tab_id == tab_id
                    && (standin.posted || standin.captured_at.elapsed() < STANDIN_FRESH)
                {
                    return Some(standin.image.clone());
                }
            }
            // 只有「此刻确实在屏幕上」的视图才拍得出画面。已经让位（或还没上屏）
            // 的视图截出来是一张空图，贴上去就是把白屏换成白屏。
            let shown = self
                .applied
                .iter()
                .any(|(id, rect)| id == tab_id && rect.is_some());
            if shown {
                if let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) {
                    match capture_preview(&tab.webview) {
                        Some(image) => {
                            self.standin = Some(StandIn {
                                tab_id: tab_id.to_string(),
                                image: image.clone(),
                                captured_at: Instant::now(),
                                posted: false,
                            });
                            return Some(image);
                        }
                        None => bridge_log(&format!("shell standin capture failed tab={tab_id}")),
                    }
                }
            }
            // 拍不到新帧：手里这张同标签的旧画面还能用就先顶上。它可能比当前页面
            // 旧几秒，但「略旧的一帧」永远好过「一块白板」——在窗口内贴上去，至少
            // 菜单是浮在真实内容上的。
            if let Some(standin) = &self.standin {
                if standin.tab_id == tab_id && standin.captured_at.elapsed() < STANDIN_STALE {
                    bridge_log(&format!(
                        "shell standin stale reuse tab={tab_id} age={}ms",
                        standin.captured_at.elapsed().as_millis()
                    ));
                    return Some(standin.image.clone());
                }
            }
            // 缓存里留着的是别的标签的旧图：丢掉，免得下一个遮挡回合把它贴到不
            // 相干的舞台上。同标签的旧图留着——下一个回合可能还用得上。
            let other_tab = self
                .standin
                .as_ref()
                .map(|standin| standin.tab_id != tab_id)
                .unwrap_or(false);
            if other_tab {
                self.standin = None;
            }
            bridge_log(&format!(
                "shell standin unavailable tab={tab_id} shown={shown}"
            ));
            None
        }

        fn post_standin(&mut self, image: String) {
            let Some(standin) = self.standin.as_mut() else {
                return;
            };
            if standin.posted {
                return;
            }
            standin.posted = true;
            bridge_log(&format!(
                "shell standin shown tab={} bytes={}",
                standin.tab_id,
                image.len()
            ));
            post_to_dashboard(
                &self.app,
                &json!({
                    "__starshipStandIn": true,
                    "visible": true,
                    "tabId": standin.tab_id,
                    "image": image,
                }),
            );
        }

        fn clear_standin(&mut self) {
            let Some(standin) = self.standin.as_mut() else {
                return;
            };
            if !standin.posted {
                return;
            }
            standin.posted = false;
            let tab_id = standin.tab_id.clone();
            bridge_log(&format!("shell standin cleared tab={tab_id}"));
            post_to_dashboard(
                &self.app,
                &json!({
                    "__starshipStandIn": true,
                    "visible": false,
                    "tabId": tab_id,
                }),
            );
        }

        /// 这个标签当前拿到的面板宽度。没上屏就没有宽度，也就无从适配。
        fn panel_width(&self, tab_id: &str) -> Option<f64> {
            self.applied
                .iter()
                .find(|(id, _)| id == tab_id)
                .and_then(|(_, rect)| *rect)
                .map(|rect| rect.width)
        }

        /// 排队一次整页适配，并为它开一个新代次（同一标签上更早排下的请求会被
        /// 丢掉，所以拖分隔条只会跑最后那一次）。
        fn schedule_fit(&mut self, tab_id: &str, width: f64, delay: Duration) {
            let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
                return;
            };
            if tab.zoom_dirty {
                return;
            }
            tab.fit_generation = tab.fit_generation.wrapping_add(1);
            let generation = tab.fit_generation;
            self.spawn_fit(tab_id, width, generation, false, delay);
        }

        /// 复测沿用同一次适配的代次：期间面板宽度又变过的话，这一发会被丢弃。
        fn schedule_fit_retry(&mut self, tab_id: &str, width: f64, generation: u64) {
            self.spawn_fit(tab_id, width, generation, true, FIT_RETRY_DELAY);
        }

        fn spawn_fit(
            &mut self,
            tab_id: &str,
            width: f64,
            generation: u64,
            retry: bool,
            delay: Duration,
        ) {
            let Ok(sender) = self.sender() else {
                return;
            };
            let tab_id = tab_id.to_string();
            let _ = thread::Builder::new()
                .name("starship-native-browser-fit".to_string())
                .spawn(move || {
                    thread::sleep(delay);
                    let _ = sender.send(Command::FitTabZoom {
                        tab_id,
                        width,
                        generation,
                        retry,
                    });
                });
        }

        /// 整页适配：中文门户/资讯站几乎都按固定桌面宽度排版（财联社 `.w-1200`），
        /// 面板比它窄，按 100% 渲染就会被切掉右边、并多出一条横向滚动条。
        /// Codex 的内嵌浏览器是把整页缩到刚好装下再渲染，这里用同一策略。
        fn fit_tab_zoom(&mut self, tab_id: &str, width: f64, generation: u64, retry: bool) {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
                return;
            };
            if self.tabs[index].fit_generation != generation || self.tabs[index].zoom_dirty {
                return;
            }
            if !(width.is_finite() && width > 1.0) {
                return;
            }
            let webview = self.tabs[index].webview.clone();
            let Some(mut zoom) = page_zoom(&webview) else {
                return;
            };

            let url = self.tabs[index].url.clone();
            let fitted = self.tabs[index].fit_width.is_some();
            let last_width = self.tabs[index].fit_width;
            let last_zoom = self.tabs[index].auto_fit;

            if !retry {
                let same_width = last_width
                    .map(|last| (width - last).abs() / last.max(1.0) <= FIT_WIDTH_TOLERANCE)
                    .unwrap_or(false);
                // 同一个站、面板也没换宽度 → 上一轮的结论依然成立，不必再量。
                // 这就是站内翻页不会闪字号的原因。
                if fitted && same_width && same_site(&self.tabs[index].fit_url, &url) {
                    return;
                }
                // 量之前必须站在已知基准上：把上一站（或上一个宽度）留下的缩放摘掉，
                // 否则量到的是「当前缩放下的视口」，而不是页面的真实排版宽度。
                if last_zoom.is_some() && (zoom - 1.0).abs() > FIT_ZOOM_EPSILON {
                    match set_page_zoom(&webview, 1.0) {
                        Some(reset) => zoom = reset,
                        None => return,
                    }
                    thread::sleep(FIT_MEASURE_SETTLE);
                }
            }

            let Some((client, scroll)) = measure_overflow(&webview) else {
                return;
            };
            if !(client.is_finite() && scroll.is_finite()) || client < 1.0 || scroll < 1.0 {
                return;
            }
            let ratio = scroll / client;

            if ratio < FIT_MIN_OVERFLOW {
                // 页面在当前缩放下装得下。这个缩放要是我们自己施加的，说明适配
                // 正在生效，保持原样；否则回到 100% —— 没有溢出的页面不该被缩小。
                let keep = matches!(last_zoom, Some(auto) if (zoom - auto).abs() <= FIT_ZOOM_EPSILON);
                self.tabs[index].fit_width = Some(width);
                self.tabs[index].fit_url = url;
                if keep {
                    return;
                }
                self.tabs[index].auto_fit = None;
                if (zoom - 1.0).abs() > FIT_ZOOM_EPSILON {
                    if set_page_zoom(&webview, 1.0).is_none() {
                        return;
                    }
                    bridge_log(&format!(
                        "auto-fit reset tab={tab_id} width={width:.0} ratio={ratio:.3} zoom={zoom:.3}->1.000"
                    ));
                }
                return;
            }

            if ratio > FIT_MAX_OVERFLOW {
                // 溢出太多：多半本来就不是桌面版（移动版页面 / 长图），
                // 缩下去只会小到没法读，不如不动。
                bridge_log(&format!(
                    "auto-fit skip tab={tab_id} width={width:.0} client={client:.0} scroll={scroll:.0} ratio={ratio:.3}"
                ));
                return;
            }

            let target = (zoom * client / scroll).clamp(FIT_MIN_ZOOM, 1.0);
            if (target - zoom).abs() > FIT_ZOOM_EPSILON {
                if set_page_zoom(&webview, target).is_none() {
                    return;
                }
            }
            self.tabs[index].auto_fit = Some(target);
            self.tabs[index].fit_width = Some(width);
            self.tabs[index].fit_url = url;
            bridge_log(&format!(
                "auto-fit apply tab={tab_id} width={width:.0} client={client:.0} scroll={scroll:.0} ratio={ratio:.3} zoom={zoom:.3}->{target:.3} retry={retry}"
            ));
            if !retry {
                // 图片/脚本晚到的站点在这一刻量到的还是半成品，隔一会儿复测一次。
                // 只复测一次，过期就丢，不会来回缩。
                self.schedule_fit_retry(tab_id, width, generation);
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

        /// 地址栏历史的落盘位置。和会话状态放同一个目录，卸载/清理时一起走。
        fn history_path(&self) -> Result<std::path::PathBuf, String> {
            Ok(self.session_path()?.with_file_name("browser-history.json"))
        }

        fn persist_history(&self) {
            let Ok(path) = self.history_path() else {
                return;
            };
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let payload = json!({ "version": 1, "entries": self.history });
            match serde_json::to_vec(&payload) {
                Ok(encoded) => {
                    if let Err(error) = std::fs::write(&path, encoded) {
                        bridge_log(&format!("history persist failed: {error}"));
                    }
                }
                Err(error) => bridge_log(&format!("history encode failed: {error}")),
            }
        }

        fn restore_history(&mut self) {
            if !self.history.is_empty() {
                return;
            }
            let Ok(path) = self.history_path() else {
                return;
            };
            let Ok(raw) = std::fs::read_to_string(&path) else {
                return;
            };
            let Ok(stored) = serde_json::from_str::<Value>(&raw) else {
                bridge_log("history restore skipped: unreadable state");
                return;
            };
            let Some(entries) = stored.get("entries").and_then(Value::as_array) else {
                return;
            };
            for entry in entries {
                let Some(url) = entry.get("url").and_then(Value::as_str) else {
                    continue;
                };
                if !valid_url(url) || url == "about:blank" {
                    continue;
                }
                self.history.push(json!({
                    "url": url,
                    "title": entry.get("title").and_then(Value::as_str).unwrap_or(""),
                    "favicon": entry.get("favicon").and_then(Value::as_str).unwrap_or(""),
                    "at": entry.get("at").and_then(Value::as_i64).unwrap_or(0),
                }));
                if self.history.len() >= HISTORY_LIMIT {
                    break;
                }
            }
            bridge_log(&format!("history restored entries={}", self.history.len()));
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

        /// 把一个标签当前的 URL/标题记进地址栏历史。
        ///
        /// 标题和图标都是导航过程中才陆续到位的，所以这个函数会被调多次：同一个
        /// URL 只保留一条，各字段用「非空覆盖空」的方式合并。
        fn record_history(&mut self, tab_id: &str) {
            let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
                return;
            };
            let url = tab.url.clone();
            if !valid_url(&url) || url == "about:blank" {
                return;
            }
            let title = tab.title.trim().to_string();
            let favicon = self
                .favicons
                .get(&origin_key(&url))
                .cloned()
                .unwrap_or_default();
            self.push_history(&url, &title, &favicon);
        }

        fn push_history(&mut self, url: &str, title: &str, favicon: &str) {
            let existing = self
                .history
                .iter()
                .position(|entry| entry.get("url").and_then(Value::as_str) == Some(url));
            let (previous_title, previous_favicon) = match existing {
                Some(index) => {
                    let entry = self.history.remove(index);
                    (
                        entry
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        entry
                            .get("favicon")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    )
                }
                None => (String::new(), String::new()),
            };
            let title = if title.trim().is_empty() {
                previous_title.clone()
            } else {
                title.to_string()
            };
            let favicon = if favicon.is_empty() {
                previous_favicon.clone()
            } else {
                favicon.to_string()
            };
            // 已经在榜首且内容没变就不落盘：一次导航会来好几个事件，标题事件
            // 和加载完成事件之间页面并不需要重写一遍历史文件。
            let unchanged = existing == Some(0)
                && title == previous_title
                && favicon == previous_favicon;
            self.history
                .insert(0, json!({ "url": url, "title": title, "favicon": favicon, "at": now_ms() }));
            if self.history.len() > HISTORY_LIMIT {
                self.history.truncate(HISTORY_LIMIT);
            }
            if !unchanged {
                self.persist_history();
            }
        }

        /// 取某个 URL 所属站点的图标，转成 data URL。
        ///
        /// 只有「此刻真的开着那个站点」才抓得到（图标得问标签页自己）。拿不到就
        /// 返回空串，由下拉层退化成首字母色块 —— 历史记录里绝大多数条目都属于
        /// 已经关掉的站点，这一层不负责去联网补。
        fn favicon_for_url(&mut self, url: &str) -> String {
            let key = origin_key(url);
            if let Some(hit) = self.favicons.get(&key) {
                return hit.clone();
            }
            let tab_id = self
                .tabs
                .iter()
                .find(|tab| tab.events > 0 && origin_key(&tab.url) == key)
                .map(|tab| tab.id.clone());
            let Some(tab_id) = tab_id else {
                return String::new();
            };
            let Some(webview) = self.webview(&tab_id) else {
                return String::new();
            };
            let data = capture_favicon_data_url(&webview).unwrap_or_default();
            if data.is_empty() {
                return data;
            }
            self.favicons.insert(key.clone(), data.clone());
            // 回填历史里同源的空图标，下次打开下拉就是现成的。
            let mut changed = false;
            for entry in self.history.iter_mut() {
                let same_site = entry
                    .get("url")
                    .and_then(Value::as_str)
                    .map(|candidate| origin_key(candidate) == key)
                    .unwrap_or(false);
                if !same_site {
                    continue;
                }
                let filled = entry
                    .get("favicon")
                    .and_then(Value::as_str)
                    .map(|value| !value.is_empty())
                    .unwrap_or(false);
                if filled {
                    continue;
                }
                if let Some(object) = entry.as_object_mut() {
                    object.insert("favicon".to_string(), json!(data));
                    changed = true;
                }
            }
            if changed {
                self.persist_history();
            }
            data
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

        /// Answer `ensure-browser` from a helper thread so the panel bootstrap
        /// never blocks tab events or geometry probes.
        fn spawn_ensure_browser(&self, id: String) {
            let app = self.app.clone();
            let result = thread::Builder::new()
                .name("starship-task-browser-ensure".to_string())
                .spawn(move || {
                    // `owned` tells the dashboard whether this profile is the
                    // shell's business at all. When the Gateway or the user owns
                    // it, the chrome layer stops retrying instead of hammering.
                    let owned = attach_only_profile().is_some();
                    let reply = match ensure_task_browser() {
                        Ok(port) => json!({ "ok": true, "port": port, "owned": true }),
                        Err(error) => json!({ "ok": false, "error": error, "owned": owned }),
                    };
                    post_to_dashboard(
                        &app,
                        &json!({ "__starshipReply": true, "id": id, "reply": reply }),
                    );
                });
            if let Err(error) = result {
                bridge_log(&format!("attach browser: worker thread failed: {error}"));
            }
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
                    // 站点弹窗：面板要知道「这个标签为什么不动了」。
                    // 只上屏元数据；放行页面的手柄留在壳层，外部拿不到。
                    if let Some(dialog) = self.dialogs.get(&tab.id) {
                        value["dialog"] = json!({
                            "kind": dialog.kind,
                            "message": dialog.message,
                            "defaultText": dialog.default_text,
                            "uri": dialog.uri,
                        });
                    }
                    value
                })
                .collect();
            post_to_dashboard(
                &self.app,
                &json!({
                    "__starshipState": true,
                    "state": {
                        "revision": self.revision,
                        "tabs": tabs,
                        // 面板可能同时显示多个标签，注入层得知道哪个是当前的，
                        // 才能把地址栏历史里那条「当前页」标出来。
                        "activeTabId": self.active_tab_id.clone(),
                        // 壳层点名要请到前台的标签：站点对同一个地址重复
                        // `window.open` 时，壳层复用已经开着的那个标签而不是再复制
                        // 一份，而官方面板只自动切「第一次见到的 native 标签」，
                        // 已有标签接不住。一次性请求，递出去就清，免得后续任何一条
                        // 标签事件把用户已经从别处切走的视图再拽回来。
                        "focusTabId": self.focus_tab_id.clone(),
                        // 下载账本。官方面板没有这一块，是星舰按「用户得知道文件
                        // 下到哪了」补的；面板照着它画下载列表。
                        "downloads": self.downloads_payload(),
                    },
                }),
            );
            self.focus_tab_id = None;
        }

        /// 把一条下载事件并进账本。返回 `true` 说明面板要重画。
        ///
        /// 归并键是壳层自己发的 `id`：`add_DownloadStarting` 只管把下载对象交出来，
        /// 不给任何能跨回调带走的标识，所以序号由 [`DOWNLOAD_SEQUENCE`] 统一发。
        fn apply_download(&mut self, event: DownloadEvent) -> bool {
            match event {
                DownloadEvent::Started {
                    id,
                    tab_id,
                    uri,
                    path,
                    total,
                } => {
                    // 同一个 id 理论上只来一次（序号只增），这里仍清一遍旧记录：
                    // 壳层在热重载或标签重建后重放事件时，列表里不该出现两条一样的下载。
                    self.downloads.retain(|entry| entry.id != id);
                    self.downloads.insert(
                        0,
                        DownloadRecord {
                            id,
                            tab_id,
                            uri,
                            path,
                            received: 0,
                            total,
                            state: COREWEBVIEW2_DOWNLOAD_STATE_IN_PROGRESS.0,
                            reason: 0,
                            started_at: now_ms(),
                        },
                    );
                    self.downloads.truncate(DOWNLOAD_LIMIT);
                    true
                }
                DownloadEvent::Progress {
                    id,
                    received,
                    total,
                } => {
                    let Some(record) = self.downloads.iter_mut().find(|entry| entry.id == id) else {
                        // 起点没记上（事件挂上之前就已经在下的那份）就没什么好更新的：
                        // 凭空造一条没有出处的记录，比少一条更糟。
                        return false;
                    };
                    record.received = received;
                    // `-1` 是「这一份没报 Content-Length」，不能拿它覆盖已知的总数。
                    if total >= 0 {
                        record.total = total;
                    }
                    true
                }
                DownloadEvent::Finished {
                    id,
                    state,
                    reason,
                    path,
                    received,
                    total,
                } => {
                    let Some(record) = self.downloads.iter_mut().find(|entry| entry.id == id) else {
                        return false;
                    };
                    record.state = state;
                    record.reason = reason;
                    if !path.is_empty() {
                        record.path = path;
                    }
                    record.received = received;
                    if total >= 0 {
                        record.total = total;
                    }
                    true
                }
            }
        }

        /// 下载账本 → 上屏用的 JSON。
        ///
        /// `state`/`reason` 在这里翻译成人话：前端不该知道
        /// `COREWEBVIEW2_DOWNLOAD_STATE_*` 的编号，壳层也不该让改了枚举值的运行库
        /// 直接漏到界面上。
        fn downloads_payload(&self) -> Vec<Value> {
            self.downloads
                .iter()
                .map(|entry| {
                    json!({
                        "id": entry.id,
                        "tabId": entry.tab_id,
                        "url": entry.uri,
                        "filename": file_name_of(&entry.path),
                        "path": entry.path,
                        "received": entry.received,
                        "total": entry.total,
                        "state": download_state_name(entry.state),
                        "reason": download_reason_name(entry.reason),
                        "startedAt": entry.started_at,
                    })
                })
                .collect()
        }

    }

    fn attach_tab_events(webview: &Webview, tab_id: &str, sender: Sender<Command>) {
        let webview = webview.clone();
        let tab_id = tab_id.to_string();
        let failure_sender = sender.clone();
        let failure_tab = tab_id.clone();
        let _ = webview.with_webview(move |platform| {
            let _ = crate::crash_log::guard("browser.with-webview.tab-events", move || {
                // 拿不到控制器 = 这个子 WebView 的 WebView2 实例根本没被建出来。
                // 这是硬失败，报回去让面板显示原因，别再让它静默地空转。
                let core = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => core,
                    Err(_) => {
                        let _ = failure_sender.send(Command::TabUnavailable {
                            tab_id: failure_tab.clone(),
                            reason: "内嵌浏览器未创建".to_string(),
                        });
                        return;
                    }
                };
                unsafe {
                // 关掉 WebView2 自带的脚本对话框。
                //
                // 默认是开着的：站点一弹 `alert`，WebView2 就在这个子视图上拉起
                // 自己的模态框 —— 它既不是面板的一部分、又没有关闭入口，而渲染
                // 进程会一直等在那儿，面板上的表现就是「这个标签死了」。关掉之后
                // `ScriptDialogOpening` 成为唯一通道，壳层攥着 deferral 代为收尾
                // （见 `PendingDialog`）。
                if let Ok(settings) = core.Settings() {
                    let _ = settings.SetAreDefaultScriptDialogsEnabled(false);
                }
                let mut token = 0i64;

                // 「人手优先」的回程通道。页面里的探针（见 `TAB_INIT_SCRIPT` 结尾）
                // 只能用 `postMessage` 把「用户刚刚在这里动过手」报上来，而子
                // WebView 的消息**不会**进 dashboard 那条 `add_WebMessageReceived`
                // —— 那是另一个 WebView2 实例的处理函数。所以每个标签都得自己
                // 接一条，报文格式也只认这一种（见 `parse_tab_human_input`）。
                let human_sender = sender.clone();
                let human_tab = tab_id.clone();
                let handler = WebMessageReceivedEventHandler::create(guarded_event(
                    "browser.tab-human-input",
                    move |_sender: Option<ICoreWebView2>,
                          args: Option<ICoreWebView2WebMessageReceivedEventArgs>| {
                        let Some(args) = args else {
                            return Ok(());
                        };
                       let mut raw = PWSTR::null();
                        if args.WebMessageAsJson(&mut raw).is_err() {
                           return Ok(());
                       }
                       if let Some(input) = parse_tab_human_input(&take_pwstr(raw)) {
                            let _ = human_sender.send(Command::TabEvent {
                                tab_id: human_tab.clone(),
                                event: TabEvent::HumanInput(input),
                            });
                        }
                        Ok(())
                    },
                ));
                let _ = core.add_WebMessageReceived(&handler, &mut token);

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

                // `clone()` 而不是直接吃下：下面还有别的监听器要用这两个值。
                let popup_sender = sender.clone();
                let popup_tab = tab_id.clone();
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

                // 站点权限请求（摄像头 / 麦克风 / 定位 / 剪贴板 / 通知……）。
                //
                // **默认拒绝**。理由不是洁癖，而是这个面板的用法：驱动它的经常
                // 是 agent 而不是人。用户在读一条新闻的时候，站点悄悄拿到摄像头
                // 或定位，是没人会预期的行为；而 WebView2 自带的权限提示框既不
                // 属于网页、也没有统一的关闭入口，等于把问题藏起来。
                //
                // 拒绝之后页面拿到的是标准的 `NotAllowedError`，脚本照常往下跑 ——
                // 比弹一个没人回答的框好。要放行只有一条路：显式写进
                // `STARSHIP_BROWSER_ALLOW_PERMISSIONS`（见 `permission_allowed`）。
                //
                // 和 `ScriptDialogOpening` 一样：**不在回调里跑消息循环、不等回包**。
                // 这里的决定是纯同步的（配置 + 站点名），所以 `SetState` 直接调用，
                // 不需要 deferral。
                let permission_tab = tab_id.clone();
                let handler = PermissionRequestedEventHandler::create(guarded_event(
                    "browser.tab-permission-requested",
                    move |_sender: Option<ICoreWebView2>,
                          args: Option<ICoreWebView2PermissionRequestedEventArgs>| {
                        let Some(args) = args else {
                            return Ok(());
                        };
                        let mut kind_value = COREWEBVIEW2_PERMISSION_KIND(0);
                        let kind = args
                            .PermissionKind(&mut kind_value)
                            .map(|_| kind_value.0)
                            .unwrap_or(0);
                        let mut raw = PWSTR::null();
                        let uri = if args.Uri(&mut raw).is_ok() {
                            take_pwstr(raw)
                        } else {
                            String::new()
                        };
                        let mut initiated = BOOL::default();
                        let user_initiated = args
                            .IsUserInitiated(&mut initiated)
                            .map(|_| initiated.as_bool())
                            .unwrap_or(false);
                        let origin = origin_key(&uri);
                        let name = permission_kind_name(kind);
                        let allowed = permission_allowed(&origin, name);
                        let _ = args.SetState(if allowed {
                            COREWEBVIEW2_PERMISSION_STATE_ALLOW
                        } else {
                            COREWEBVIEW2_PERMISSION_STATE_DENY
                        });
                        if permission_should_log(&origin, name) {
                            bridge_log(&format!(
                                "shell permission tab={permission_tab} kind={name} \
                                 user_initiated={user_initiated} allowed={allowed} origin={origin}"
                            ));
                        }
                        Ok(())
                    },
                ));
                let _ = core.add_PermissionRequested(&handler, &mut token);

                // 下载。官方面板根本没有下载 UI：工具菜单里那条「下载文件夹」只把
                // `Browser.setDownloadBehavior` 设成 allow，再把资源管理器拉起来 ——
                // 用户既看不到进度，也不知道文件最终落在哪。这里把 WebView2 的下载
                // 事件接上，面板才能画出一份真实的下载列表。
                //
                // 和上面几个回调同一条铁律：**不跑消息循环、不等回包** —— 全是同步
                // 属性读，读完就往 worker 投一条命令。
                //
                // `add_DownloadStarting` 不在 `ICoreWebView2` 上，而在 `ICoreWebView2_4`
                // （WebView2 的接口按版本往上叠：IUnknown → ICoreWebView2 → _2 → _3 →
                // _4），所以要先把控制器给的那份 cast 上去。cast 失败只说明这台机器
                // 上的 WebView2 运行库太老、没有下载事件，面板少一条列表，别的照常，
                // 因此这里不 early-return——下面还有弹窗要挂。
                match core.cast::<ICoreWebView2_4>() {
                    Ok(download_core) => {
                        let download_sender = sender.clone();
                        let download_tab = tab_id.clone();
                        let handler = DownloadStartingEventHandler::create(guarded_event(
                            "browser.tab-download-starting",
                            move |_sender: Option<ICoreWebView2>,
                                  args: Option<ICoreWebView2DownloadStartingEventArgs>| {
                                let Some(args) = args else {
                                    return Ok(());
                                };
                                // 拿不到下载对象就什么都不做，让 WebView2 按它自己的
                                // 默认落点收尾 —— 总比把这一次下载整个吞掉强。
                                let Ok(operation) = args.DownloadOperation() else {
                                    return Ok(());
                                };
                                let id = DOWNLOAD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                                let mut uri_raw = PWSTR::null();
                                let uri = if operation.Uri(&mut uri_raw).is_ok() {
                                    take_pwstr(uri_raw)
                                } else {
                                    String::new()
                                };
                                let mut path_raw = PWSTR::null();
                                let mut path = if operation.ResultFilePath(&mut path_raw).is_ok() {
                                    take_pwstr(path_raw)
                                } else {
                                    String::new()
                                };
                                // 落点由星舰定：工具菜单那条「下载文件夹」打开的就是这个
                                // 目录，列表上写的路径得是文件真正在的地方，两处不能各说
                                // 各话。默认路径不为空说明站点自己指定过（另存为流程），
                                // 那种情况尊重它。
                                if path.trim().is_empty() {
                                    if let Ok(directory) = download_directory(None) {
                                        let name = file_name_of(&uri);
                                        if !name.is_empty() {
                                            let candidate = directory.join(name);
                                            let display = candidate.to_string_lossy().to_string();
                                            // `SetResultFilePath` 收 `PCWSTR`，
                                            // `&HSTRING` 正好是 `Param<PCWSTR>` 的一份实现。
                                            if args
                                                .SetResultFilePath(&HSTRING::from(display.clone()))
                                                .is_ok()
                                            {
                                                path = display;
                                            }
                                        }
                                    }
                                }
                                let mut total = -1i64;
                                let _ = operation.TotalBytesToReceive(&mut total);
                                // 星舰自己那份列表就是下载 UI，别让 WebView2 再弹一个
                                // 自带的下载提示，否则同一个下载在屏幕上有两个说法。
                                let _ = args.SetHandled(true);
                                let _ = download_sender.send(Command::Download {
                                    event: DownloadEvent::Started {
                                        id,
                                        tab_id: download_tab.clone(),
                                        uri,
                                        path,
                                        total,
                                    },
                                });

                                // 进度。挂在这个下载对象自己身上，事件一响只投一条命令；
                                // 抽稀在 `download_progress_due` 里做，别让大文件把 worker
                                // 的队列灌满。
                                let progress_sender = download_sender.clone();
                                let progress_handler = BytesReceivedChangedEventHandler::create(
                                    guarded_event(
                                        "browser.tab-download-progress",
                                        move |operation: Option<ICoreWebView2DownloadOperation>,
                                              _args: Option<IUnknown>| {
                                            let Some(operation) = operation else {
                                                return Ok(());
                                            };
                                            if !download_progress_due(id, Instant::now()) {
                                                return Ok(());
                                            }
                                            let mut received = 0i64;
                                            let mut total = -1i64;
                                            let _ = operation.BytesReceived(&mut received);
                                            let _ = operation.TotalBytesToReceive(&mut total);
                                            let _ = progress_sender.send(Command::Download {
                                                event: DownloadEvent::Progress {
                                                    id,
                                                    received,
                                                    total,
                                                },
                                            });
                                            Ok(())
                                        },
                                    ),
                                );
                                let mut progress_token = 0i64;
                                let _ = operation
                                    .add_BytesReceivedChanged(&progress_handler, &mut progress_token);

                                // 终态。`StateChanged` 进 in-progress 时也会响一次，
                                // 只有走到中断或完成才收尾。
                                let finish_sender = download_sender.clone();
                                let finish_handler = StateChangedEventHandler::create(guarded_event(
                                    "browser.tab-download-state",
                                    move |operation: Option<ICoreWebView2DownloadOperation>,
                                          _args: Option<IUnknown>| {
                                        let Some(operation) = operation else {
                                            return Ok(());
                                        };
                                        let mut state = COREWEBVIEW2_DOWNLOAD_STATE_IN_PROGRESS;
                                        if operation.State(&mut state).is_err() {
                                            return Ok(());
                                        }
                                        if state == COREWEBVIEW2_DOWNLOAD_STATE_IN_PROGRESS {
                                            return Ok(());
                                        }
                                        let mut reason_raw = 0i32;
                                        let mut reason =
                                            COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON(reason_raw);
                                        let _ = operation.InterruptReason(&mut reason);
                                        reason_raw = reason.0;
                                        let mut received = 0i64;
                                        let mut total = -1i64;
                                        let _ = operation.BytesReceived(&mut received);
                                        let _ = operation.TotalBytesToReceive(&mut total);
                                        let mut path_raw = PWSTR::null();
                                        let path = if operation.ResultFilePath(&mut path_raw).is_ok() {
                                            take_pwstr(path_raw)
                                        } else {
                                            String::new()
                                        };
                                        // 闸门记录只在这时候清：进度回调自己不知道下载
                                        // 是不是最后一次响。
                                        download_progress_forget(id);
                                        let _ = finish_sender.send(Command::Download {
                                            event: DownloadEvent::Finished {
                                                id,
                                                state: state.0,
                                                reason: reason_raw,
                                                path,
                                                received,
                                                total,
                                            },
                                        });
                                        Ok(())
                                    },
                                ));
                                let mut finish_token = 0i64;
                                let _ =
                                    operation.add_StateChanged(&finish_handler, &mut finish_token);
                                Ok(())
                            },
                        ));
                        let _ = download_core.add_DownloadStarting(&handler, &mut token);
                    }
                    Err(_) => {
                        bridge_log("browser.tab-download: ICoreWebView2_4 unavailable");
                    }
                }

                // 站点弹窗（`alert`/`confirm`/`prompt`/`beforeunload`）。
                //
                // 默认对话框已经在上面关掉了，这是唯一能看到它们的入口。要点只有
                // 一个：**必须先拿 deferral**。handler 一返回，WebView2 就认为这个
                // 弹窗处理完了、页面立刻继续跑，壳层再也插不上手；攥着 deferral
                // 就等于把「什么时候放行」的决定权收到了壳层手里。
                //
                // 这里绝对不能跑消息循环或等回包（微软文档明确警告）：回调本身就
                // 在 WebView2 的 UI 线程上，堵住它等于堵住整个面板。所以只做
                // 「取名、记账、往 worker 投一条命令」。
                let dialog_sender = sender.clone();
                let dialog_tab = tab_id.clone();
                let handler = ScriptDialogOpeningEventHandler::create(guarded_event(
                    "browser.tab-script-dialog",
                    move |_sender: Option<ICoreWebView2>,
                          args: Option<ICoreWebView2ScriptDialogOpeningEventArgs>| {
                        let Some(args) = args else {
                            return Ok(());
                        };
                        let Ok(deferral) = args.GetDeferral() else {
                            // 拿不到 deferral 就只能让 WebView2 按自己的方式收尾，
                            // 好过留一个永远不 Complete 的空壳。
                            return Ok(());
                        };
                        let mut kind_value = COREWEBVIEW2_SCRIPT_DIALOG_KIND(0);
                        let kind = match args.Kind(&mut kind_value) {
                            Ok(()) => match kind_value.0 {
                                1 => "confirm",
                                2 => "prompt",
                                3 => "beforeunload",
                                _ => "alert",
                            },
                            Err(_) => "alert",
                        }
                        .to_string();
                        let mut raw = PWSTR::null();
                        let message = if args.Message(&mut raw).is_ok() {
                            take_pwstr(raw)
                        } else {
                            String::new()
                        };
                        let mut raw = PWSTR::null();
                        let default_text = if args.DefaultText(&mut raw).is_ok() {
                            take_pwstr(raw)
                        } else {
                            String::new()
                        };
                        let mut raw = PWSTR::null();
                        let uri = if args.Uri(&mut raw).is_ok() {
                            take_pwstr(raw)
                        } else {
                            String::new()
                        };
                        // 同一个标签又冒出一个弹窗（前一个还没人处理）：旧的那个
                        // 不能就这么丢掉，它的 deferral 不 Complete 页面就永远卡在
                        // 上一轮。按「取消」放行旧的那个，把位置让给新的。
                        let replaced = PENDING_DIALOGS.with(|map| {
                            map.borrow_mut().insert(
                                dialog_tab.clone(),
                                PendingDialog { args, deferral },
                            )
                        });
                        if let Some(previous) = replaced {
                            let _ = previous.deferral.Complete();
                        }
                        // 跨线程摘要要和「攥住 deferral」同一刻落账：动作回执是在
                        // worker 线程上拼的，而下面这条 `Command::ScriptDialog`
                        // 只能排在队尾 —— 等 worker 取到它，「点一下把页面挡在弹窗
                        // 里」的那次动作早就超时返回了。摘要先落，失败的动作才有
                        // 东西可翻译成 `COMPUTER_DIALOG_BLOCKED`。
                        note_pending_dialog(
                            &dialog_tab,
                            PendingDialogSummary {
                                kind: kind.clone(),
                                message: message.clone(),
                                default_text: default_text.clone(),
                                uri: uri.clone(),
                                raised_at: Instant::now(),
                            },
                        );
                        let _ = dialog_sender.send(Command::ScriptDialog {
                            tab_id: dialog_tab.clone(),
                            kind,
                            message,
                            default_text,
                            uri,
                        });
                        Ok(())
                    },
                ));
                let _ = core.add_ScriptDialogOpening(&handler, &mut token);
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
            "(() => { window.__starshipObservation = (window.__starshipObservation || 0) + 1; \
             return { width: window.innerWidth, height: window.innerHeight, \
             observation: window.__starshipObservation }; })()"
                .to_string(),
        )
        .ok_or_else(|| "Native browser snapshot failed".to_string())?;
        let metrics: Value = serde_json::from_str(&raw)
            .map_err(|_| "Native browser snapshot failed".to_string())?;
        let width = metrics.get("width").and_then(Value::as_f64).unwrap_or(0.0);
        let height = metrics.get("height").and_then(Value::as_f64).unwrap_or(0.0);
        let observation = metrics
            .get("observation")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if width <= 0.0 || height <= 0.0 {
            return Err("Native browser tab is not visible".to_string());
        }
        let data = capture_png(webview)?;
        Ok(json!({
            "ok": true,
            "dataUrl": format!("data:image/png;base64,{data}"),
            "cssWidth": width,
            "cssHeight": height,
            "observationId": format!("obs-{observation}"),
        }))
    }

    /// 老 runtime 没有原生 Find 接口时的哨兵错误：只有它触发 `window.find()` 回退。
    const NO_NATIVE_FIND: &str = "no-native-find";

    /// 页内查找（在标签页自己的 webview 上执行）。
    ///
    /// 为什么不再用 `Page.findInPage`：**WebView2 的 CDP 桥里没有这个方法** —— 实测回
    /// `'Page.findInPage' wasn't found`；更糟的是老实现把它包成了 `ok:true`，于是查找条
    /// 输入之后既不高亮也不计数，界面上完全看不出来它坏了。改用 WebView2 原生
    /// `ICoreWebView2_28::Find()`：它能给出匹配总数与当前序号，UI 的 `n/m` 才有数。
    ///
    /// 老 runtime 缺这组接口时退回 `window.find()`：只回答「找到没找到」，回执里如实写
    /// `counted:false` —— 宁可少给信息，也不假装有计数。
    fn find_in_page(webview: &Webview, text: &str, message: &Value) -> Result<Value, String> {
        let forward = message
            .get("forward")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let find_next = message
            .get("findNext")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let match_case = message
            .get("matchCase")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if text.is_empty() {
            let _ = stop_find(webview);
            return Ok(json!({
                "find": {
                    "term": "",
                    "matches": 0,
                    "activeMatchOrdinal": 0,
                    "found": false,
                    "counted": true,
                    "engine": "WebView2Find",
                }
            }));
        }
        match native_find(webview, text, forward, find_next, match_case) {
            Ok(reply) => Ok(json!({ "find": reply })),
            Err(error) if error == NO_NATIVE_FIND => {
                let found = fallback_find(webview, text, forward)?;
                Ok(json!({
                    "find": {
                        "term": text,
                        "matches": Value::Null,
                        "activeMatchOrdinal": Value::Null,
                        "found": found,
                        "counted": false,
                        "engine": "window.find",
                        "note": "当前 WebView2 runtime 没有原生 Find 接口，只能回答找到没找到",
                    }
                }))
            }
            Err(error) => Err(error),
        }
    }

    /// 停掉查找并撤掉页面上的高亮。
    fn stop_find(webview: &Webview) -> Result<(), String> {
        let (sender, receiver) = mpsc::channel();
        webview
            .with_webview(move |platform| {
                let _ = crate::crash_log::guard("browser.find.stop", move || {
                    let outcome = (|| -> Result<(), String> {
                        let core = unsafe { platform.controller().CoreWebView2() }
                            .map_err(|error| format!("CoreWebView2 unavailable: {error}"))?;
                        let owner = core
                            .cast::<ICoreWebView2_28>()
                            .map_err(|_| NO_NATIVE_FIND.to_string())?;
                        let find = unsafe { owner.Find() }
                            .map_err(|error| format!("Find unavailable: {error}"))?;
                        unsafe { find.Stop() }
                            .map_err(|error| format!("Find stop failed: {error}"))?;
                        Ok(())
                    })();
                    let _ = sender.send(outcome);
                });
            })
            .map_err(|error| format!("WebView2 unavailable: {error}"))?;
        match receiver
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "find stop timed out".to_string())?
        {
            Ok(()) => Ok(()),
            // 没有原生 Find 的 runtime 上，什么都没高亮过，停停也是成功了。
            Err(error) if error == NO_NATIVE_FIND => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// 走一遍原生查找，返回 `{matches, activeMatchOrdinal, ...}`。
    ///
    /// 关键约束：**`with_webview` 的闭包跑在 WebView2 的 UI 线程上，而 `Start` 的完成
    /// 回调也排在同一条线程上** —— 在闭包里等完成回调会把自己的回调饿死（必然超时）。
    /// 所以闭包只负责发起，等待（以及随后的读计数）都留在调用方线程上。
    fn native_find(
        webview: &Webview,
        text: &str,
        forward: bool,
        find_next: bool,
        match_case: bool,
    ) -> Result<Value, String> {
        if find_next {
            let (done_sender, _done_receiver) = mpsc::channel::<bool>();
            issue_find(webview, None, forward, true, false, done_sender)?;
            // 序号是异步更新的；给它一帧的时间，别急着读回旧值。
            thread::sleep(Duration::from_millis(60));
        } else {
            let (done_sender, done_receiver) = mpsc::channel::<bool>();
            issue_find(webview, Some(text), forward, false, match_case, done_sender)?;
            let _ = done_receiver.recv_timeout(Duration::from_millis(1500));
        }
        let (matches, active) = read_find_counts(webview)?;
        Ok(json!({
            "term": text,
            "matches": matches,
            "activeMatchOrdinal": if matches > 0 { active + 1 } else { 0 },
            "found": matches > 0,
            "counted": true,
            "engine": "WebView2Find",
        }))
    }

    /// 发起查找（或上/下一个）。`done` 只在「新查找」这条路上用：`Start` 完成时它会被
    /// 唤醒，调用方据此决定什么时候读计数。
    fn issue_find(
        webview: &Webview,
        text: Option<&str>,
        forward: bool,
        find_next: bool,
        match_case: bool,
        done: mpsc::Sender<bool>,
    ) -> Result<(), String> {
        let (sender, receiver) = mpsc::channel();
        let term = HSTRING::from(text.unwrap_or_default().to_string());
        webview
            .with_webview(move |platform| {
                let _ = crate::crash_log::guard("browser.find.issue", move || {
                    let outcome = (|| -> Result<(), String> {
                        let core = unsafe { platform.controller().CoreWebView2() }
                            .map_err(|error| format!("CoreWebView2 unavailable: {error}"))?;
                        let owner = core
                            .cast::<ICoreWebView2_28>()
                            .map_err(|_| NO_NATIVE_FIND.to_string())?;
                        let find = unsafe { owner.Find() }
                            .map_err(|error| format!("Find unavailable: {error}"))?;
                        if find_next {
                            let stepped = if forward {
                                unsafe { find.FindNext() }
                            } else {
                                unsafe { find.FindPrevious() }
                            };
                            stepped.map_err(|error| format!("FindNext failed: {error}"))?;
                            return Ok(());
                        }
                        let environment = platform.environment();
                        let env = environment
                            .cast::<ICoreWebView2Environment15>()
                            .map_err(|_| NO_NATIVE_FIND.to_string())?;
                        let options = unsafe { env.CreateFindOptions() }
                            .map_err(|error| format!("CreateFindOptions failed: {error}"))?;
                        unsafe { options.SetFindTerm(&term) }
                            .map_err(|error| format!("SetFindTerm failed: {error}"))?;
                        let _ = unsafe { options.SetIsCaseSensitive(match_case) };
                        let _ = unsafe { options.SetShouldHighlightAllMatches(true) };
                        let handler = FindStartCompletedHandler::create(Box::new(
                            move |error: windows::core::Result<()>| {
                                let _ = done.send(error.is_ok());
                                Ok(())
                            },
                        ));
                        unsafe { find.Start(&options, &handler) }
                            .map_err(|error| format!("Find start failed: {error}"))?;
                        Ok(())
                    })();
                    let _ = sender.send(outcome);
                });
            })
            .map_err(|error| format!("WebView2 unavailable: {error}"))?;
        receiver
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "find issue timed out".to_string())?
    }

    /// 读一次匹配总数与当前序号（序号是 0 基）。
    fn read_find_counts(webview: &Webview) -> Result<(i32, i32), String> {
        let (sender, receiver) = mpsc::channel();
        webview
            .with_webview(move |platform| {
                let _ = crate::crash_log::guard("browser.find.counts", move || {
                    let outcome = (|| -> Result<(i32, i32), String> {
                        let core = unsafe { platform.controller().CoreWebView2() }
                            .map_err(|error| format!("CoreWebView2 unavailable: {error}"))?;
                        let owner = core
                            .cast::<ICoreWebView2_28>()
                            .map_err(|_| NO_NATIVE_FIND.to_string())?;
                        let find = unsafe { owner.Find() }
                            .map_err(|error| format!("Find unavailable: {error}"))?;
                        let mut matches = 0i32;
                        let mut active = 0i32;
                        let _ = unsafe { find.MatchCount(&mut matches) };
                        let _ = unsafe { find.ActiveMatchIndex(&mut active) };
                        Ok((matches, active))
                    })();
                    let _ = sender.send(outcome);
                });
            })
            .map_err(|error| format!("WebView2 unavailable: {error}"))?;
        receiver
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "find counts timed out".to_string())?
    }

    /// 没有原生 Find 时的兜底：`window.find()` 只能回答「这一下找到没有」。
    fn fallback_find(webview: &Webview, text: &str, forward: bool) -> Result<bool, String> {
        let script = format!(
            "String(window.find && window.find({}, false, {}))",
            js_literal(&json!(text)),
            if forward { "false" } else { "true" },
        );
        let raw = execute_script(webview, script)
            .ok_or_else(|| "window.find failed".to_string())?;
        Ok(raw.trim() == "true")
    }

    /// 单页最多返回多少个元素。上限是防御性的：无限滚动列表那种页面一次全量
    /// 返回会把回包撑到几 MB，反而让上层看不清，也让 CDP 回包变得难读。
    const MAX_ELEMENT_PAGE: usize = 200;
    /// `semantic_v2` 的默认页大小。多带了 role/状态/bounds，单元素更贵，所以
    /// 比 `dom_refs_v1` 小一档。
    const SEMANTIC_ELEMENT_PAGE: usize = 120;
    /// `dom_refs_v1` 的默认页大小（既有调用方的口径，保持原值不变）。
    const DOM_REFS_ELEMENT_PAGE: usize = 200;

    /// 元素扫描脚本。用占位符替换而不是 `format!` 拼字符串：脚本里全是花括号，
    /// 每加一个 `{` 都要写成 `{{` 的话，改一处脚本就得赌一次转义。
    ///
    /// 两件事在同一段脚本里：**观测序号**（每次「看一眼」往前走一格，动作带着
    /// 它回来；对不上说明页面在观察之后变了，上层据此重读而不是盲点一下）和
    /// **分页**（`continuation` 是同一次观测的下一页，带着它回来时不重新记账，
    /// 于是上一页的 `ref` 仍然有效）。
    const ELEMENTS_SCRIPT: &str = r#"(() => {
  const offset = __OFFSET__, limit = __LIMIT__, semantic = __SEMANTIC__, expect = __EXPECT__;
  const format = __FORMAT__;
  function implicitRole(el) {
    const tag = el.tagName.toLowerCase();
    const type = (el.getAttribute("type") || "").toLowerCase();
    if (tag === "a") { return el.hasAttribute("href") ? "link" : "generic"; }
    if (tag === "button") { return "button"; }
    if (tag === "select") { return "combobox"; }
    if (tag === "textarea") { return "textbox"; }
    if (tag === "input") {
      if (type === "checkbox") { return "checkbox"; }
      if (type === "radio") { return "radio"; }
      if (type === "range") { return "slider"; }
      if (type === "number") { return "spinbutton"; }
      if (type === "file") { return "file-input"; }
      if (type === "submit" || type === "button" || type === "reset") { return "button"; }
      return "textbox";
    }
    if (el.isContentEditable) { return "textbox"; }
    return el.getAttribute("role") || "generic";
  }
  if (expect !== null) {
    const current = window.__starshipObservation || 0;
    // 直接回对象，不要 `JSON.stringify`：`ExecuteScript` 已经会把返回值序列化成
    // JSON，再包一层字符串等于把清单变成「字符串里的 JSON」，调用方读
    // `reply.elements` 只会拿到 undefined（这一条踩过一次，探针 8 个断言全红）。
    if (current !== expect) { return { stale: true, observation: current }; }
  } else {
    window.__starshipObservation = (window.__starshipObservation || 0) + 1;
  }
  const observation = window.__starshipObservation || 0;
  if (!window.__starshipRefSeq) { window.__starshipRefSeq = 0; }
  const selector =
    "a,button,input,textarea,select,[role=button],[role=link],[role=textbox],[contenteditable=true],[tabindex]";
  const nodes = Array.from(document.querySelectorAll(selector));
  const items = [];
  let seen = 0;
  let more = false;
  for (const el of nodes) {
    const rect = el.getBoundingClientRect();
    if (rect.width < 2 || rect.height < 2) { continue; }
    const style = window.getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden" || Number(style.opacity || "1") === 0) {
      continue;
    }
    if (seen < offset) { seen++; continue; }
    if (items.length >= limit) { more = true; break; }
    seen++;
    let ref = el.getAttribute("data-starship-ref");
    if (!ref) {
      ref = "sr-" + (++window.__starshipRefSeq);
      el.setAttribute("data-starship-ref", ref);
    }
    const item = {
      ref: ref,
      tag: el.tagName.toLowerCase(),
      role: el.getAttribute("role") || implicitRole(el),
      name: (el.getAttribute("aria-label") || el.getAttribute("title") || el.textContent || "")
        .replace(/\s+/g, " ").trim().slice(0, 120),
      value: typeof el.value === "string" ? el.value.slice(0, 120) : null,
      disabled: el.disabled === true,
      rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
    };
    if (semantic) {
      item.type = el.getAttribute("type") || "";
      item.placeholder = el.getAttribute("placeholder") || "";
      item.description = (el.getAttribute("aria-description") || "").slice(0, 120);
      item.checked = typeof el.checked === "boolean" ? el.checked : null;
      item.selected = typeof el.selected === "boolean" ? el.selected : null;
      item.expanded = el.getAttribute("aria-expanded");
      item.focused = document.activeElement === el;
      item.inViewport =
        rect.bottom > 0 && rect.top < window.innerHeight &&
        rect.right > 0 && rect.left < window.innerWidth;
    }
    items.push(item);
  }
  const next = offset + items.length;
  const result = {
    count: items.length,
    elements: items,
    observationId: "obs-" + observation,
    format: format,
    offset: offset,
    truncated: more,
    continuation: more ? ("sv2:" + observation + ":" + next) : null,
  };
  if (semantic) {
    result.viewport = {
      width: window.innerWidth,
      height: window.innerHeight,
      scrollX: Math.round(window.scrollX || 0),
      scrollY: Math.round(window.scrollY || 0),
    };
    result.url = location.href;
    result.title = document.title;
  }
  return result;
})()"#;

    /// `continuation` 令牌：`sv2:<观测序号>:<偏移>`。
    ///
    /// 令牌里带着观测序号，是为了让「翻到第二页」这件事可以被证伪：页面在两次
    /// 读取之间导航了，序号就对不上，这时候返回官方的 `COMPUTER_STALE_OBSERVATION`
    /// 比默默给出一份错位的清单有用得多。偏移量封顶只是拒绝明显畸形的输入。
    fn parse_continuation(token: &str) -> Result<(usize, Option<u64>), String> {
        let malformed = || format!("Malformed continuation token: {token}");
        let rest = token.strip_prefix("sv2:").ok_or_else(malformed)?;
        let (observation, offset) = rest.split_once(':').ok_or_else(malformed)?;
        let observation: u64 = observation.parse().map_err(|_| malformed())?;
        let offset: usize = offset.parse().map_err(|_| malformed())?;
        if offset > 10_000 {
            return Err(malformed());
        }
        Ok((offset, Some(observation)))
    }

    /// 当前页面的元素清单。
    ///
    /// `snapshotFormat` 对齐官方 `get_browser_state` 的两种形态：
    /// `dom_refs_v1`（壳层一路走来的 `ref` + 基本元数据）与 `semantic_v2`
    /// （多 role/状态/bounds，并且**分页**）。默认仍是前者，因为它是既有
    /// 调用方的口径，换掉等于把旧调用一起改掉。
    fn elements(webview: &Webview, message: &Value) -> Result<Value, String> {
        let format = message
            .get("snapshotFormat")
            .or_else(|| message.get("format"))
            .and_then(Value::as_str)
            .unwrap_or("dom_refs_v1");
        let semantic = match format {
            "dom_refs_v1" => false,
            "semantic_v2" => true,
            other => return Err(format!("Unsupported snapshot format: {other}")),
        };
        let limit = message
            .get("maxElements")
            .and_then(Value::as_u64)
            .map(|value| (value.max(1) as usize).min(MAX_ELEMENT_PAGE))
            .unwrap_or(if semantic {
                SEMANTIC_ELEMENT_PAGE
            } else {
                DOM_REFS_ELEMENT_PAGE
            });
        let (offset, expected) = match message.get("continuation").and_then(Value::as_str) {
            Some(token) => parse_continuation(token)?,
            None => (0_usize, None),
        };
        let script = ELEMENTS_SCRIPT
            .replace("__OFFSET__", &offset.to_string())
            .replace("__LIMIT__", &limit.to_string())
            .replace("__SEMANTIC__", if semantic { "true" } else { "false" })
            .replace(
                "__EXPECT__",
                &expected
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "null".to_string()),
            )
            .replace("__FORMAT__", &format!("\"{format}\""));
        let raw = execute_script(webview, script)
            .ok_or_else(|| "Native browser element scan failed".to_string())?;
        let mut value: Value = serde_json::from_str(&raw)
            .map_err(|_| "Native browser element scan failed".to_string())?;
        // 兜底：脚本若回了一层字符串（历史版本这么干过），在这里解回来，
        // 免得调用方拿到一个读不出 `elements` 的字符串还当成功。
        if let Value::String(inner) = &value {
            if let Ok(parsed) = serde_json::from_str::<Value>(inner) {
                value = parsed;
            }
        }
        if value.get("stale").and_then(Value::as_bool).unwrap_or(false) {
            let current = value.get("observation").and_then(Value::as_u64).unwrap_or(0);
            return Ok(json!({
                "ok": false,
                "code": "COMPUTER_STALE_OBSERVATION",
                "observationId": format!("obs-{current}"),
                "error": "The page changed since this continuation was issued; read the panel again",
            }));
        }
        Ok(value)
    }

    /// 页面在当前缩放下的视口宽度（含纵向滚动条之外的可用宽度）和内容真正需要
    /// 的宽度，单位都是 CSS px。
    ///
    /// `clientWidth` 与 `scrollWidth` 之比就是要除掉的溢出倍数 —— 财联社桌面上
    /// 实测是 902 / 1200 = 0.7517，与手工调出来的最佳缩放完全一致。
    fn measure_overflow(webview: &Webview) -> Option<(f64, f64)> {
        let script = r#"(() => {
  const d = document.documentElement;
  const b = document.body;
  const client = d ? (d.clientWidth || 0) : 0;
  const scroll = Math.max(d ? (d.scrollWidth || 0) : 0, b ? (b.scrollWidth || 0) : 0);
  return { client: client, scroll: scroll };
})()"#;
        let raw = execute_script(webview, script.to_string())?;
        let value: Value = serde_json::from_str(&raw).ok()?;
        Some((
            value.get("client").and_then(Value::as_f64)?,
            value.get("scroll").and_then(Value::as_f64)?,
        ))
    }

    /// 两个 URL 是不是同一个站。站内翻页沿用上一页的适配结果，这样每点一条
    /// 新闻不会重新量一次 —— 量一次就得先把缩放摘回 100%，视觉上会闪字号。
    fn same_site(left: &str, right: &str) -> bool {
        if left.is_empty() || right.is_empty() {
            return false;
        }
        match (Url::parse(left), Url::parse(right)) {
            (Ok(left), Ok(right)) => left.host_str() == right.host_str(),
            _ => left == right,
        }
    }

    fn page_state(webview: &Webview) -> Value {
        let script = r#"(() => {
  const el = document.activeElement;
  return {
    url: location.href,
    title: document.title,
    scrollX: window.scrollX,
    scrollY: window.scrollY,
    observationId: "obs-" + (window.__starshipObservation || 0),
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

    /// 一次 CDP 调用。**把失败当失败返回，而不是藏进「成功」的回包里。**
    ///
    /// WebView2 用 `HRESULT` 报告「这个方法不存在 / 这次调用失败」。老实现把 error 丢掉、
    /// 只把 result 交出去，于是两件本该一眼看见的事都变成了谜：
    ///   * `Page.findInPage` 在 WebView2 的 CDP 桥里**根本不存在** —— 回包却是 `ok:true`，
    ///     错误只以 `{"message": "'Page.findInPage' wasn't found"}` 的形式躺在 result 里；
    ///   * `Page.captureScreenshot` 偶尔拿不到帧 —— 只回一句「截图失败」，日志里一个字没有。
    /// 现在错误带上方法名 / HRESULT / 应答体，并且**一律写进 `native-browser.log`**，
    /// 这样即使调用方不回执，也查得出真因。
    fn call_cdp(webview: &Webview, method: &str, params: &str) -> Result<String, String> {
        let (sender, receiver) = mpsc::channel();
        let method_name = method.to_string();
        let label = method_name.clone();
        let method = HSTRING::from(method_name.clone());
        let parameters = HSTRING::from(params.to_string());
        webview
            .with_webview(move |platform| {
                let _ = crate::crash_log::guard("browser.with-webview.cdp", move || {
                    let core = match unsafe { platform.controller().CoreWebView2() } {
                        Ok(core) => core,
                        Err(error) => {
                            let _ = sender.send(Err(format!(
                                "{method_name}: CoreWebView2 unavailable: {error}"
                            )));
                            return;
                        }
                    };
                    let handler_sender = sender.clone();
                    let handler_method = method_name.clone();
                    let handler = CallDevToolsProtocolMethodCompletedHandler::create(
                        guarded_completed(
                            "browser.cdp-completed",
                            move |error: windows::core::Result<()>, result: String| {
                            let outcome = if error.is_ok() {
                                Ok(result)
                            } else {
                                Err(format!("{handler_method}: {error:?}; body={result}"))
                            };
                            let _ = handler_sender.send(outcome);
                            Ok(())
                            },
                        ),
                    );
                    if unsafe { core.CallDevToolsProtocolMethod(&method, &parameters, &handler) }
                        .is_err()
                    {
                        let _ = sender.send(Err(format!(
                            "{method_name}: CallDevToolsProtocolMethod rejected"
                        )));
                    }
                });
            })
            .map_err(|error| format!("{label}: WebView2 unavailable: {error}"))?;
        let outcome = receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| format!("{label}: CDP call timed out after 10s"))?;
        if let Err(error) = &outcome {
            bridge_log(&format!("cdp failed: {error}"));
        }
        outcome
    }

    /// 只关心「成没成」的调用方。
    fn cdp_ok(webview: &Webview, method: &str, params: &str) -> Result<(), String> {
        call_cdp(webview, method, params).map(|_| ())
    }

    /// 只关心「结果 JSON」的调用方。
    fn cdp_json(webview: &Webview, method: &str, params: &str) -> Result<Value, String> {
        let raw = call_cdp(webview, method, params)?;
        serde_json::from_str::<Value>(&raw)
            .map_err(|error| format!("{method}: unreadable CDP result: {error}"))
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

    /// 元素落点的页面侧脚本。占位符 `__REF__` 由 Rust 侧替换，JS 里不留拼接。
    ///
    /// 为什么不是「矩形几何中心」：站点常把标题 `<div>` 压在 `<a>` 上面（财联社首页
    /// 头条就是这种版式），几何中心命中的是那个 DIV，`closest("a")` 为空 —— 点击回执
    /// 照样 `effect:confirmed`，但链接收不到这一下，页面纹丝不动（实测三次复现）。
    ///
    /// 策略：中心优先（绝大多数元素与旧行为完全一致），中心被别的层盖住时以中心为
    /// 原点向外做网格采样，取第一枚「命中元素属于目标」的点；一枚都找不到就退回中心 ——
    /// 该退化的地方退化，但绝不因为找不到干净落点就让动作失败。
    ///
    /// 返回 `{x, y, onTarget, hitTag, hitHref, probes}`；元素不存在时返回 `null`。
    /// 回执带上命中信息，是因为 `effect:confirmed` 只说明「事件发出去了」，
    /// 上层要靠 `onTarget` / `hitTag` / `hitHref` 才能判断这一下有没有点歪。
    const ELEMENT_POINT_SCRIPT: &str = r##"(() => {
  const ref = __REF__;
  const el = document.querySelector('[data-starship-ref="' + ref + '"]');
  if (!el) { return null; }
  // 只在「它完全不在视口里」时才滚，而且横向用 `nearest`、滚动用 `instant`。
  //
  // 两条都是实测踩出来的：`inline:"center"` 在比面板宽的页面上会把页面横向拖走
  // （财联社首页把目标从 x=461 拖到 x=-370，落点直接跑到屏幕外）；而站点自己的
  // `scroll-behavior:smooth` 会让滚动变成动画，紧接着量到的 rect 还是滚动前的
  // 位置。元素已经在视口里时**一律不滚** —— 元素清单刚给过坐标，它就在那儿。
  const visibleBox = (box) => ({
    left: Math.max(box.left, 0),
    top: Math.max(box.top, 0),
    right: Math.min(box.right, window.innerWidth),
    bottom: Math.min(box.bottom, window.innerHeight),
  });
  const measure = () => el.getBoundingClientRect();
  let rect = measure();
  if (rect.width <= 0 || rect.height <= 0) { return null; }
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  const intersects = (box) => box.bottom > 0 && box.right > 0 && box.top < vh && box.left < vw;
  if (!intersects(rect)) {
    el.scrollIntoView({ block: "center", inline: "nearest", behavior: "instant" });
    rect = measure();
    if (rect.width <= 0 || rect.height <= 0) { return null; }
  }
  // 落点范围只取「框 ∩ 视口」：元素被裁掉一半时，人点的也是看得见的那一半。
  const view = visibleBox(rect);
  if (view.right - view.left < 2 || view.bottom - view.top < 2) {
    // 怎么都进不了视口（固定容器、iframe 之类）：退回旧行为，只给个几何中心。
    return {
      x: rect.x + rect.width / 2,
      y: rect.y + rect.height / 2,
      onTarget: null,
      hitTag: null,
      hitHref: null,
      probes: 0,
    };
  }
  const cx = (view.left + view.right) / 2;
  const cy = (view.top + view.bottom) / 2;
  const halfX = Math.max((view.right - view.left) / 2, 0);
  const halfY = Math.max((view.bottom - view.top) / 2, 0);
  const anchorOf = (node) => {
    try { return node && node.closest ? node.closest("a[href]") : null; } catch (error) { return null; }
  };
  const report = (x, y, hit, probes) => {
    const anchor = anchorOf(hit);
    return {
      x: x,
      y: y,
      onTarget: !!hit,
      hitTag: hit && hit.tagName ? hit.tagName.toLowerCase() : null,
      hitHref: anchor ? anchor.href : null,
      probes: probes,
    };
  };
  let probes = 0;
  const tryAt = (x, y) => {
    if (x < 0 || y < 0 || x > vw || y > vh) { return null; }
    probes += 1;
    const hit = document.elementFromPoint(x, y);
    if (!hit) { return null; }
    if (hit === el || el.contains(hit)) { return hit; }
    try {
      if (hit.closest('[data-starship-ref="' + ref + '"]') === el) { return hit; }
    } catch (error) { /* 命中链读不到就按没命中处理 */ }
    return null;
  };
  const atCenter = tryAt(cx, cy);
  if (atCenter) { return report(cx, cy, atCenter, probes); }
  const stepX = Math.max(4, Math.min(24, (halfX * 2) / 8));
  const stepY = Math.max(3, Math.min(16, (halfY * 2) / 4));
  const SIGNS = [[1, 1], [1, -1], [-1, 1], [-1, -1]];
  for (let oy = 0; oy <= halfY; oy += stepY) {
    for (let ox = 0; ox <= halfX; ox += stepX) {
      if (oy === 0 && ox === 0) { continue; }
      for (let at = 0; at < SIGNS.length; at += 1) {
        const px = cx + SIGNS[at][0] * ox;
        const py = cy + SIGNS[at][1] * oy;
        const hit = tryAt(px, py);
        if (hit) { return report(px, py, hit, probes); }
      }
      if (probes > 600) { break; }
    }
    if (probes > 600) { break; }
  }
  return report(cx, cy, null, probes);
})()"##;

    /// 元素落点。返回值同时带着命中诊断（见 `ELEMENT_POINT_SCRIPT` 的说明）。
    fn element_point(webview: &Webview, reference: &str) -> Result<Value, String> {
        let script = ELEMENT_POINT_SCRIPT.replace("__REF__", &js_literal(&json!(reference)));
        let raw =
            execute_script(webview, script).ok_or_else(|| "Element lookup failed".to_string())?;
        let value: Value =
            serde_json::from_str(&raw).map_err(|_| "Element lookup failed".to_string())?;
        if value.is_null() || value.get("x").and_then(Value::as_f64).is_none() {
            return Err(format!("Element reference {reference} was not found"));
        }
        Ok(value)
    }

    /// 落点解析的完整结果：`{x, y, onTarget, hitTag, hitHref, probes}`。
    /// 走坐标时没有命中诊断可给，那几个字段如实留空。
    fn act_point(webview: &Webview, message: &Value) -> Result<Value, String> {
        if let Some(reference) = message.get("elementRef").and_then(Value::as_str) {
            if !valid_element_ref(reference) {
                return Err("Invalid element reference".to_string());
            }
            return element_point(webview, reference);
        }
        let (x, y) = finite_point(message)
            .ok_or_else(|| "A valid elementRef or x/y point is required".to_string())?;
        Ok(json!({
            "x": x,
            "y": y,
            "onTarget": Value::Null,
            "hitTag": Value::Null,
            "hitHref": Value::Null,
            "probes": Value::Null,
        }))
    }

    /// 只要坐标的调用方（悬停 / 拖拽 / 打字前聚焦）走这个。
    fn act_point_xy(webview: &Webview, message: &Value) -> Result<(f64, f64), String> {
        let value = act_point(webview, message)?;
        let x = value
            .get("x")
            .and_then(Value::as_f64)
            .ok_or_else(|| "A valid elementRef or x/y point is required".to_string())?;
        let y = value
            .get("y")
            .and_then(Value::as_f64)
            .ok_or_else(|| "A valid elementRef or x/y point is required".to_string())?;
        Ok((x, y))
    }

    /// 一次 `mouseMoved`。悬停与「首帧兜底」共用同一条命令，差别只在发几次。
    fn hover_at(webview: &Webview, x: f64, y: f64) -> Result<(), String> {
        let params = json!({
            "type": "mouseMoved",
            "x": x,
            "y": y,
            "button": "none",
            "clickCount": 0,
        });
        cdp_ok(webview, "Input.dispatchMouseEvent", &cdp_params(params))?;
        Ok(())
    }

    /// 光标底下那一下到底落上没有：`document.querySelectorAll(":hover")` 里出现
    /// 命中元素自身、它的祖先或它的后代，就算落上。页面答不上话（正在导航、渲染
    /// 进程忙）时按「落上了」处理 —— 兜底不该因为读不到状态就永远发两遍。
    fn hover_landed(webview: &Webview, x: f64, y: f64) -> bool {
        let script = format!(
            r#"(() => {{
  const hit = document.elementFromPoint({x}, {y});
  if (!hit) {{ return true; }}
  const chain = [];
  let node = hit;
  while (node) {{ chain.push(node); node = node.parentNode; }}
  const hovered = document.querySelectorAll(":hover");
  for (let at = 0; at < hovered.length; at += 1) {{
    if (chain.indexOf(hovered[at]) !== -1) {{ return true; }}
  }}
  return false;
}})()"#
        );
        let Some(raw) = execute_script(webview, script) else {
            return true;
        };
        serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(true)
    }

    /// 合成事件的脚本模板。占位符在 Rust 侧替换，JS 里不留任何拼接，
    /// 页面上跑到的永远是完整字面量。`fire` 吞掉页面处理函数自己抛的错：
    /// 站点脚本炸了不该让「点一下」这件事变成壳层失败。
    const DOM_EVENT_CLICK: &str = r##"(() => {
  const ref = __REF__;
  const point = __POINT__;
  const el = ref
    ? document.querySelector('[data-starship-ref="' + ref + '"]')
    : (point ? document.elementFromPoint(point.x, point.y) : null);
  if (!el) { return { ok: false, reason: "target-not-found" }; }
  const rect = el.getBoundingClientRect();
  const x = point ? point.x : rect.x + rect.width / 2;
  const y = point ? point.y : rect.y + rect.height / 2;
  const target = el.closest("button,a,input,textarea,select,[role=button],[role=link]") || el;
  if (target.focus) {
    try { target.focus({ preventScroll: true }); }
    catch (error) { try { target.focus(); } catch (ignored) { /* 焦点抢不到也照样派发 */ } }
  }
  const base = {
    bubbles: true, cancelable: true, composed: true,
    clientX: x, clientY: y, screenX: x, screenY: y,
    button: __BUTTON__, buttons: __BUTTONS__, detail: __COUNT__,
  };
  const pointer = { pointerId: 1, pointerType: "mouse", isPrimary: true };
  let delivered = 0;
  const fire = (event) => {
    try { target.dispatchEvent(event); delivered += 1; }
    catch (error) { /* 页面自己抛错不该拖垮动作 */ }
  };
  fire(new PointerEvent("pointerdown", Object.assign({}, pointer, base)));
  fire(new MouseEvent("mousedown", base));
  fire(new PointerEvent("pointerup", Object.assign({}, pointer, base)));
  fire(new MouseEvent("mouseup", base));
  fire(new MouseEvent("click", base));
  return { ok: true, delivered: delivered, tag: target.tagName.toLowerCase(), x: x, y: y };
})()"##;

    const DOM_EVENT_TYPE: &str = r##"(() => {
  const ref = __REF__;
  const point = __POINT__;
  const text = __TEXT__;
  let el = ref ? document.querySelector('[data-starship-ref="' + ref + '"]') : null;
  if (!el && point) { el = document.elementFromPoint(point.x, point.y); }
  if (!el) { el = document.activeElement; }
  if (!el) { return { ok: false, reason: "target-not-found" }; }
  const target = el.closest ? (el.closest("input,textarea,[contenteditable=true]") || el) : el;
  if (target.focus) {
    try { target.focus({ preventScroll: true }); }
    catch (error) { try { target.focus(); } catch (ignored) { /* 焦点抢不到也照样派发 */ } }
  }
  let applied = text;
  if (typeof target.value === "string") {
    const proto = target instanceof HTMLTextAreaElement
      ? HTMLTextAreaElement.prototype
      : HTMLInputElement.prototype;
    const descriptor = Object.getOwnPropertyDescriptor(proto, "value");
    if (descriptor && descriptor.set) { descriptor.set.call(target, text); }
    else { target.value = text; }
    applied = target.value;
  } else if (target.isContentEditable) {
    target.textContent = text;
    applied = target.textContent;
  }
  const fire = (event) => {
    try { target.dispatchEvent(event); } catch (error) { /* 见 DOM_EVENT_CLICK */ }
  };
  fire(new InputEvent("input", {
    bubbles: true, composed: true, data: text, inputType: "insertText",
  }));
  fire(new Event("change", { bubbles: true }));
  return { ok: true, value: applied, length: text.length };
})()"##;

    /// `<select>` 的选中。原生下拉展开后那层列表是操作系统画的，CDP 的鼠标
    /// 事件够不着它（点得到控件、点不到选项），所以这里和 `scroll` 一样只有
    /// 合成事件这一条路：直接改 `selectedIndex`，再把 `input`/`change` 派给页面。
    ///
    /// `value` / `label` / `index` 三个入口只认一个。两个以上同时给出就是调用方
    /// 自己没说清想要哪一个，报错比替他猜一个安全 —— 猜错会静默改错选项。
    const DOM_EVENT_SELECT: &str = r##"(() => {
  const ref = __REF__;
  const point = __POINT__;
  const want = __WANT__;
  let el = ref ? document.querySelector('[data-starship-ref="' + ref + '"]') : null;
  if (!el && point) { el = document.elementFromPoint(point.x, point.y); }
  if (!el) { return { ok: false, reason: "target-not-found" }; }
  const target = el.closest ? (el.closest("select") || el) : el;
  if (target.tagName !== "SELECT") { return { ok: false, reason: "not-a-select" }; }
  if (target.disabled) { return { ok: false, reason: "select-disabled" }; }
  const options = Array.prototype.slice.call(target.options || []);
  if (!options.length) { return { ok: false, reason: "select-has-no-options" }; }
  const values = options.map((option) => String(option.value));
  const labels = options.map((option) => String(option.textContent || "").trim());
  let index = -1;
  if (want.kind === "index") {
    index = want.value;
    if (!(index >= 0) || index >= options.length) {
      return { ok: false, reason: "index-out-of-range:" + index + "/" + options.length };
    }
  } else if (want.kind === "value") {
    index = values.indexOf(String(want.value));
    if (index < 0) { return { ok: false, reason: "value-not-found:" + String(want.value) }; }
  } else {
    const wanted = String(want.value).trim();
    index = labels.indexOf(wanted);
    if (index < 0) {
      const lowered = wanted.toLowerCase();
      for (let at = 0; at < labels.length; at += 1) {
        if (labels[at].toLowerCase() === lowered) { index = at; break; }
      }
    }
    if (index < 0) { return { ok: false, reason: "label-not-found:" + wanted }; }
  }
  if (target.focus) {
    try { target.focus({ preventScroll: true }); }
    catch (error) { try { target.focus(); } catch (ignored) { /* 见 DOM_EVENT_CLICK */ } }
  }
  const previous = target.selectedIndex;
  target.selectedIndex = index;
  if (!options[index].selected) { options[index].selected = true; }
  const fire = (event) => {
    try { target.dispatchEvent(event); } catch (error) { /* 见 DOM_EVENT_CLICK */ }
  };
  fire(new Event("input", { bubbles: true, composed: true }));
  fire(new Event("change", { bubbles: true }));
  const chosen = options[target.selectedIndex] || options[index];
  return {
    ok: true,
    previous: previous,
    index: target.selectedIndex,
    selected: target.selectedIndex,
    value: chosen ? String(chosen.value) : null,
    label: chosen ? String(chosen.textContent || "").trim() : null,
    options: options.slice(0, 50).map((option, at) => ({
      index: at,
      value: String(option.value),
      label: String(option.textContent || "").trim(),
      selected: at === target.selectedIndex,
    })),
  };
})()"##;

    const DOM_EVENT_KEY: &str = r##"(() => {
  const el = document.activeElement || document.body;
  if (!el) { return { ok: false, reason: "no-target" }; }
  const key = __KEY__;
  const init = {
    key: key, code: __CODE__, bubbles: true, cancelable: true, composed: true,
  };
  const fire = (type) => {
    const event = new KeyboardEvent(type, init);
    Object.defineProperty(event, "keyCode", { get: () => __VK__ });
    Object.defineProperty(event, "which", { get: () => __VK__ });
    try { el.dispatchEvent(event); } catch (error) { /* 见 DOM_EVENT_CLICK */ }
  };
  fire("keydown");
  if (__HAS_TEXT__) { fire("keypress"); }
  fire("keyup");
  return { ok: true, key: key };
})()"##;

    /// 滚动的合成事件模板。跟 `DOM_EVENT_CLICK` 走同一套占位符约定。
    ///
    /// 两段式：先派发 `wheel`（站点的监听器这才拿得到手势，虚拟列表、
    /// 自定义滚动容器都靠它），再回头量一次位置 —— 页面没人接手时脚本
    /// 自己补上 `scrollTop/scrollLeft`。合成事件本身不带滚动这个默认动作，
    /// 所以第二段在真实网页上是常态，而页面自己 `preventDefault` 接管时
    /// 就老老实实不动，避免一次动作滚出两倍的距离。
    const DOM_EVENT_SCROLL: &str = r##"(() => {
  const ref = __REF__;
  const point = __POINT__;
  const deltaX = __DX__;
  const deltaY = __DY__;
  const wantsX = Math.abs(deltaX) > 0.5;
  const wantsY = Math.abs(deltaY) > 0.5;
  const start = ref
    ? document.querySelector('[data-starship-ref="' + ref + '"]')
    : (point ? document.elementFromPoint(point.x, point.y) : null);
  if (ref && !start) { return { ok: false, reason: "target-not-found" }; }
  const doc = document.scrollingElement || document.documentElement;
  const isDoc = (node) => node === doc || node === document.documentElement || node === document.body;
  const scrollsOn = (node, axis) => {
    if (!node || !node.getBoundingClientRect) { return false; }
    const style = getComputedStyle(node);
    const overflow = axis === "y" ? style.overflowY : style.overflowX;
    const hasRoom = axis === "y"
      ? node.scrollHeight > node.clientHeight + 1
      : node.scrollWidth > node.clientWidth + 1;
    return hasRoom && (isDoc(node) || /(auto|scroll|overlay)/.test(overflow));
  };
  const chain = [];
  for (let node = start; node; node = node.parentElement) { chain.push(node); }
  chain.push(doc);
  let scroller = null;
  for (const node of chain) {
    const xOk = !wantsX || scrollsOn(node, "x");
    const yOk = !wantsY || scrollsOn(node, "y");
    if (xOk && yOk) { scroller = isDoc(node) ? doc : node; break; }
  }
  if (!scroller) { scroller = doc; }
  const position = () => chain.map((node) => (node.scrollTop || 0) + (node.scrollLeft || 0));
  const before = position();
  const target = start || scroller;
  const rect = target.getBoundingClientRect ? target.getBoundingClientRect() : null;
  const clientX = point ? point.x : (rect ? rect.x + rect.width / 2 : 0);
  const clientY = point ? point.y : (rect ? rect.y + rect.height / 2 : 0);
  let delivered = 0;
  let prevented = false;
  const event = new WheelEvent("wheel", {
    bubbles: true, cancelable: true, composed: true,
    clientX: clientX, clientY: clientY, deltaX: deltaX, deltaY: deltaY, deltaMode: 0,
  });
  try { target.dispatchEvent(event); delivered = 1; }
  catch (error) { /* 站点监听器自己抛错不该把「滚一下」判成壳层失败 */ }
  prevented = event.defaultPrevented === true;
  const moved = (snapshot) => snapshot.some((value, index) => Math.abs(value - before[index]) > 0.5);
  let route = "wheel_event";
  if (!moved(position()) && !prevented) {
    scroller.scrollLeft = (scroller.scrollLeft || 0) + deltaX;
    scroller.scrollTop = (scroller.scrollTop || 0) + deltaY;
    route = "scroll_to";
  }
  const after = position();
  const scrolled = moved(after);
  if (!scrolled && route === "scroll_to") { route = "scroll_noop"; }
  return {
    ok: true,
    route: route,
    scroller: isDoc(scroller) ? "document" : scroller.tagName.toLowerCase(),
    delivered: delivered,
    prevented: prevented,
    moved: scrolled,
    scrollX: scroller.scrollLeft || 0,
    scrollY: scroller.scrollTop || 0,
    deltaX: deltaX,
    deltaY: deltaY,
  };
})()"##;

    /// 当前页面的观测序号。`None` 表示这一刻问不到页面（正在导航或渲染），
    /// 这时不做陈旧判定 —— 把失败留给动作本身去报，别用壳层的猜测挡住动作。
    fn observation_token(webview: &Webview) -> Option<String> {
        let raw = execute_script(
            webview,
            "({ observation: window.__starshipObservation || 0 })".to_string(),
        )?;
        let value: Value = serde_json::from_str(&raw).ok()?;
        let seq = value.get("observation").and_then(Value::as_u64)?;
        Some(format!("obs-{seq}"))
    }

    /// 把 JSON 值原样嵌进脚本文本。字符串的转义交给 serde，所以元素引用、
    /// 文本、选择器都逃不出这个字面量去改脚本。
    fn js_literal(value: &Value) -> String {
        serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
    }

    /// `drag` 的起点/终点。既认 `{from:{elementRef}, to:{x,y}}` 这种成对写法，
    /// 也认 `{elementRef:"sr-1", to:{x,y}}` 这种「从某元素拖到某点」的混写：
    /// 上层手里往往只有起点是元素，落点是个坐标。
    fn drag_endpoint(webview: &Webview, message: &Value, key: &str) -> Result<(f64, f64), String> {
        if let Some(nested) = message.get(key) {
            if nested.is_object() {
                return act_point_xy(webview, nested);
            }
        }
        if key == "from" {
            return act_point_xy(webview, message);
        }
        Err(format!("A `{key}` elementRef or x/y point is required"))
    }

    /// 元素引用或坐标，编译成合成事件脚本能用的两个字面量。
    fn dom_event_target(message: &Value) -> Result<(String, String), String> {
        if let Some(reference) = message.get("elementRef").and_then(Value::as_str) {
            if !valid_element_ref(reference) {
                return Err("Invalid element reference".to_string());
            }
            return Ok((js_literal(&json!(reference)), "null".to_string()));
        }
        let (x, y) = finite_point(message)
            .ok_or_else(|| "A valid elementRef or x/y point is required".to_string())?;
        Ok(("null".to_string(), json!({ "x": x, "y": y }).to_string()))
    }

    fn dom_event_result(webview: &Webview, script: String, failure: &str) -> Result<Value, String> {
        let raw = execute_script(webview, script).ok_or_else(|| failure.to_string())?;
        let value: Value = serde_json::from_str(&raw).map_err(|_| failure.to_string())?;
        match value.get("ok").and_then(Value::as_bool) {
            Some(true) => Ok(value),
            _ => Err(format!(
                "{failure}: {}",
                value
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )),
        }
    }

    /// 合成事件通道（`inputRoute:"dom_event"`）。
    ///
    /// CDP 的 `Input.*` 走的是浏览器真实输入栈，最接近人；但有的站点用
    /// `isTrusted` 之外的依据判断手势（比如要求事件由页面脚本自己派发、
    /// 或者把真实输入栈上的合成序列丢掉），这条路把动作翻译成页面里的
    /// 合成事件，作为退化通道与可信通道并存，由调用方二选一。
    fn dom_event_click(webview: &Webview, message: &Value) -> Result<Value, String> {
        let (reference, point) = dom_event_target(message)?;
        let button = message.get("button").and_then(Value::as_str).unwrap_or("left");
        let (button_index, buttons) = match button {
            "left" => (0, 1),
            "right" => (2, 2),
            "middle" => (1, 4),
            _ => return Err("Invalid mouse button".to_string()),
        };
        let click_count = message
            .get("clickCount")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 3);
        let script = DOM_EVENT_CLICK
            .replace("__REF__", &reference)
            .replace("__POINT__", &point)
            .replace("__BUTTON__", &button_index.to_string())
            .replace("__BUTTONS__", &buttons.to_string())
            .replace("__COUNT__", &click_count.to_string());
        dom_event_result(webview, script, "Synthetic click failed")
    }

    fn dom_event_type(webview: &Webview, message: &Value) -> Result<Value, String> {
        let text = message
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| "A text value is required".to_string())?;
        if text.len() > 20_000 {
            return Err("Text value is too large".to_string());
        }
        let (reference, point) = dom_event_target(message)?;
        let script = DOM_EVENT_TYPE
            .replace("__REF__", &reference)
            .replace("__POINT__", &point)
            .replace("__TEXT__", &js_literal(&json!(text)));
        dom_event_result(webview, script, "Synthetic input failed")
    }

    /// `select` 要选哪一个。`value` / `label` / `index` 三选一，多给一个就报错
    /// （见 `DOM_EVENT_SELECT` 的说明）。长度上限只是为了挡住明显畸形的输入。
    fn select_want(message: &Value) -> Result<Value, String> {
        let mut given: Vec<&str> = Vec::new();
        if message.get("value").is_some() {
            given.push("value");
        }
        if message.get("label").is_some() {
            given.push("label");
        }
        if message.get("index").is_some() {
            given.push("index");
        }
        if given.is_empty() {
            return Err("A select requires one of value, label, or index".to_string());
        }
        if given.len() > 1 {
            return Err(format!(
                "A select takes exactly one of value, label, or index (got {})",
                given.join(", ")
            ));
        }
        match given[0] {
            "value" => {
                let value = message
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "A select value must be a string".to_string())?;
                if value.len() > 500 {
                    return Err("Select value is too long".to_string());
                }
                Ok(json!({ "kind": "value", "value": value }))
            }
            "label" => {
                let label = message
                    .get("label")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "A select label must be a string".to_string())?;
                if label.len() > 500 {
                    return Err("Select label is too long".to_string());
                }
                Ok(json!({ "kind": "label", "value": label }))
            }
            _ => {
                let index = message
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "A select index must be a non-negative integer".to_string())?;
                if index > 10_000 {
                    return Err("Select index is out of range".to_string());
                }
                Ok(json!({ "kind": "index", "value": index }))
            }
        }
    }

    fn select_option(webview: &Webview, message: &Value) -> Result<Value, String> {
        let want = select_want(message)?;
        let (reference, point) = dom_event_target(message)?;
        let script = DOM_EVENT_SELECT
            .replace("__REF__", &reference)
            .replace("__POINT__", &point)
            .replace("__WANT__", &js_literal(&want));
        dom_event_result(webview, script, "Select failed")
    }

    fn dom_event_key(webview: &Webview, message: &Value) -> Result<Value, String> {
        let key = message
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| "A key value is required".to_string())?;
        let Some((code, virtual_key, text)) = key_definition(key) else {
            return Err(format!("Unsupported key: {key}"));
        };
        let script = DOM_EVENT_KEY
            .replace("__KEY__", &js_literal(&json!(key)))
            .replace("__CODE__", &js_literal(&json!(code)))
            .replace("__VK__", &virtual_key.to_string())
            .replace("__HAS_TEXT__", if text.is_empty() { "false" } else { "true" });
        dom_event_result(webview, script, "Synthetic key failed")
    }

    /// 一「格」滚轮折成多少 CSS 像素。Chrome 自己的 wheel 步长就是 100px，
    /// `scrollAmount` 在这个数上做倍数，官方 CUA 契约的「轮齿」才有落点。
    const SCROLL_PX_PER_TICK: f64 = 100.0;

    /// `scroll` 的位移。三种写法都认：`deltaX/deltaY`（`browser_pointer` 契约）、
    /// `dx/dy`（上层偶发简写）、`scrollDirection + scrollAmount`（官方
    /// `computer.act` 的 CUA 契约）。三种都不给就报错 —— 静默滑一个零会让
    /// 上层以为「滚过了」，比直接失败更难查。
    fn scroll_delta(message: &Value) -> Result<(f64, f64), String> {
        let number = |key: &str| message.get(key).and_then(Value::as_f64);
        let has_delta = ["deltaX", "deltaY", "dx", "dy"]
            .iter()
            .any(|key| message.get(key).is_some());
        let mut delta_x = number("deltaX").or_else(|| number("dx")).unwrap_or(0.0);
        let mut delta_y = number("deltaY").or_else(|| number("dy")).unwrap_or(0.0);
        match message.get("scrollDirection").and_then(Value::as_str) {
            Some(direction) => {
                let amount = message
                    .get("scrollAmount")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.0);
                if !amount.is_finite() || amount <= 0.0 {
                    return Err("scrollAmount must be a positive number".to_string());
                }
                let distance = amount * SCROLL_PX_PER_TICK;
                match direction {
                    "up" => delta_y -= distance,
                    "down" => delta_y += distance,
                    "left" => delta_x -= distance,
                    "right" => delta_x += distance,
                    other => return Err(format!("Unsupported scrollDirection: {other}")),
                }
            }
            None if !has_delta => {
                return Err(
                    "A scroll requires deltaX/deltaY, dx/dy, or a scrollDirection".to_string(),
                );
            }
            None => {}
        }
        if !delta_x.is_finite() || !delta_y.is_finite() {
            return Err("Invalid scroll delta".to_string());
        }
        if delta_x.abs() > 1_000_000.0 || delta_y.abs() > 1_000_000.0 {
            return Err("Scroll delta is out of range".to_string());
        }
        Ok((delta_x, delta_y))
    }

    /// `scroll` 的落点可以缺省 —— 既没有元素也没有坐标时就滚页面主滚动容器。
    /// 点击/输入必须点名目标，滚动不必：顶部/底部一类的动作本来就没有目标元素。
    fn dom_event_optional_target(message: &Value) -> Result<(String, String), String> {
        let named = message.get("elementRef").and_then(Value::as_str).is_some()
            || message.get("x").and_then(Value::as_f64).is_some();
        if !named {
            return Ok(("null".to_string(), "null".to_string()));
        }
        dom_event_target(message)
    }

    /// `scroll` 走 DOM 通道，而且是**唯一**通道：WebView2 上
    /// `Input.dispatchMouseEvent{type:"mouseWheel"}` 实测永不完回执（10 秒后
    /// 只能判失败），`Input.synthesizeScrollGesture` 也一路空转不回。可信输入
    /// 栈这条路上没有可用的滚动，只能把动作翻译成页面里的合成事件 + 直接改
    /// 滚动位置，理由与 `dom_event_click` 那段一致。
    ///
    /// 回执里的 `inputRoute` 据此如实写 `dom_event`：即便调用方点的是
    /// `trusted`，真实发生的也是这条退化路径，报成 trusted 就是假账。
    fn dom_event_scroll(webview: &Webview, message: &Value) -> Result<Value, String> {
        let (delta_x, delta_y) = scroll_delta(message)?;
        let (reference, point) = dom_event_optional_target(message)?;
        let script = DOM_EVENT_SCROLL
            .replace("__REF__", &reference)
            .replace("__POINT__", &point)
            .replace("__DX__", &delta_x.to_string())
            .replace("__DY__", &delta_y.to_string());
        dom_event_result(webview, script, "Synthetic scroll failed")
    }

    /// 输入通道。默认 `trusted`（走 CDP 真实输入栈），`dom_event` 是退化通道。
    fn input_route(message: &Value) -> Result<&'static str, String> {
        match message.get("inputRoute").and_then(Value::as_str) {
            None => Ok("trusted"),
            Some("trusted") | Some("cdp") | Some("trusted_cdp") => Ok("trusted"),
            Some("dom_event") | Some("dom-event") | Some("synthetic") => Ok("dom_event"),
            Some(other) => Err(format!("Unsupported inputRoute: {other}")),
        }
    }

    /// 壳层认识的动作全集。表在这里而不是靠 `perform_act` 的兜底分支回答，
    /// 是为了让「不认识的动作」能在派发层就被判成契约不匹配，而不是跑完一圈
    /// 才从字符串里看出问题。
    fn known_act_action(action: &str) -> bool {
        const ACTIONS: [&str; 25] = [
            "click",
            "hover",
            "move",
            "drag",
            "scroll",
            "type",
            "select",
            "key",
            "press",
            "wait",
            "screenshot",
            "snapshot",
            "elements",
            "navigate",
            "back",
            "forward",
            "reload",
            "stop",
            "zoom",
            "devtools",
            "find",
            "findStop",
            "downloads",
            "upload",
            "dialog",
        ];
        ACTIONS.contains(&action)
    }

    /// 文件上传：把本地绝对路径交给页面的 `<input type=file>`。
    ///
    /// 星舰没有资源仓储，也就不该假装有 —— 参数直接收本机路径，先校验成
    /// 绝对路径再交给 CDP，免得相对路径按进程工作目录解析出意料之外的文件。
    fn upload_target_object(webview: &Webview, message: &Value) -> Result<String, String> {
        let expression = match message.get("elementRef").and_then(Value::as_str) {
            Some(reference) => {
                if !valid_element_ref(reference) {
                    return Err("Invalid element reference".to_string());
                }
                format!(
                    "document.querySelector('[data-starship-ref=\"' + {} + '\"]')",
                    js_literal(&json!(reference))
                )
            }
            None => {
                let selector = message
                    .get("selector")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "An elementRef or selector is required".to_string())?;
                if selector.is_empty() || selector.len() > 300 {
                    return Err("Invalid selector".to_string());
                }
                format!("document.querySelector({})", js_literal(&json!(selector)))
            }
        };
        let params = json!({ "expression": expression, "returnByValue": false });
        let value: Value = cdp_json(webview, "Runtime.evaluate", &cdp_params(params))?;
        value
            .get("result")
            .and_then(|result| result.get("objectId"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "The file input was not found".to_string())
    }

    /// 校验一份文件清单：绝对路径、长度合理、条数有上限。
    fn upload_files(message: &Value) -> Result<Vec<String>, String> {
        let files = message
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| "A files array is required".to_string())?;
        if files.is_empty() || files.len() > 32 {
            return Err("Between 1 and 32 files are required".to_string());
        }
        let mut resolved = Vec::with_capacity(files.len());
        for entry in files {
            let path = entry
                .as_str()
                .ok_or_else(|| "Every file must be a path string".to_string())?;
            if path.is_empty() || path.len() > 4096 || path.contains('\0') {
                return Err("Invalid file path".to_string());
            }
            if !std::path::PathBuf::from(path).is_absolute() {
                return Err(format!("File path must be absolute: {path}"));
            }
            if !std::path::PathBuf::from(path).is_file() {
                return Err(format!("File does not exist: {path}"));
            }
            resolved.push(path.to_string());
        }
        Ok(resolved)
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
            cdp_ok(webview, "Input.dispatchMouseEvent", &cdp_params(params))?;
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

    /// `replace: true` 的实现：把焦点元素里已有的内容选中，接下来的插入就是替换。
    ///
    /// 为什么不是「发一个 Ctrl+A」：那是把选择权交给当前焦点，落在
    /// `contenteditable` 上会选中整篇、落在 `type=number` 上直接抛异常；而且它
    /// 把一段本来可信的插入变成了「能不能选中全看页面」。这里只改选区，不碰
    /// 输入通道 —— 文本仍然走 `Input.insertText`，可信输入栈没有被打折。
    ///
    /// 返回的是**落在哪条路上**（`input:12` / `contenteditable` / `unsupported`），
    /// 好让调用方分辨「真的替换了」还是「页面里根本没有可替换的东西」。
    fn select_all_text(webview: &Webview) -> Result<String, String> {
        let script = r#"(() => {
  const el = document.activeElement;
  if (!el || el === document.body || el === document.documentElement) { return "no-focus"; }
  try {
    if (typeof el.setSelectionRange === "function" && typeof el.value === "string") {
      el.setSelectionRange(0, el.value.length);
      return "input:" + el.value.length;
    }
  } catch (error) { /* number/email 之类没有选区，退到下一条路 */ }
  if (el.isContentEditable) {
    const range = document.createRange();
    range.selectNodeContents(el);
    const selection = window.getSelection();
    if (selection) {
      selection.removeAllRanges();
      selection.addRange(range);
      return "contenteditable";
    }
  }
  return "unsupported";
})()"#;
        let raw = execute_script(webview, script.to_string())
            .ok_or_else(|| "Could not clear the field".to_string())?;
        serde_json::from_str::<String>(&raw).map_err(|_| "Could not clear the field".to_string())
    }

    /// 逐键输入的按键间隔，毫秒。
    ///
    /// 默认 20ms 是「像人，但不像人那么慢」。上限 200ms 是给会被页面识别的
    /// 输入框留的手动旋钮；总时长封顶 15 秒，是为了让一个几千字的字符串
    /// 不会把一次工具调用拖成好几分钟 —— 超了就按比例压间隔，逐键这件事
    /// 本身不变（0 间隔仍然是每个字符一次 `keydown`）。
    fn keystroke_delay(message: &Value, text: &str) -> u64 {
        const DEFAULT_MS: u64 = 20;
        const MAX_MS: u64 = 200;
        const TOTAL_BUDGET_MS: u64 = 15_000;
        let requested = message
            .get("delayMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MS)
            .min(MAX_MS);
        let count = text.chars().count() as u64;
        if count == 0 || requested.saturating_mul(count) <= TOTAL_BUDGET_MS {
            return requested;
        }
        (TOTAL_BUDGET_MS / count).min(MAX_MS)
    }

    /// 一个字符对应的物理按键：`（code, 虚拟键码, 是否需要 Shift）`。
    ///
    /// 只覆盖美式键盘打得出来的部分。返回 `None` 的字符（中文、emoji）没有
    /// 对应的物理键，硬编一个假键码只会让页面上的按键判断更乱 —— 调用方会
    /// 把它们退回 `Input.insertText`。
    fn keystroke_key(character: char) -> Option<(&'static str, i64, bool)> {
        let definition = match character {
            'a'..='z' => (
                match character {
                    'a' => "KeyA",
                    'b' => "KeyB",
                    'c' => "KeyC",
                    'd' => "KeyD",
                    'e' => "KeyE",
                    'f' => "KeyF",
                    'g' => "KeyG",
                    'h' => "KeyH",
                    'i' => "KeyI",
                    'j' => "KeyJ",
                    'k' => "KeyK",
                    'l' => "KeyL",
                    'm' => "KeyM",
                    'n' => "KeyN",
                    'o' => "KeyO",
                    'p' => "KeyP",
                    'q' => "KeyQ",
                    'r' => "KeyR",
                    's' => "KeyS",
                    't' => "KeyT",
                    'u' => "KeyU",
                    'v' => "KeyV",
                    'w' => "KeyW",
                    'x' => "KeyX",
                    'y' => "KeyY",
                    _ => "KeyZ",
                },
                character.to_ascii_uppercase() as i64,
                false,
            ),
            'A'..='Z' => (
                match character {
                    'A' => "KeyA",
                    'B' => "KeyB",
                    'C' => "KeyC",
                    'D' => "KeyD",
                    'E' => "KeyE",
                    'F' => "KeyF",
                    'G' => "KeyG",
                    'H' => "KeyH",
                    'I' => "KeyI",
                    'J' => "KeyJ",
                    'K' => "KeyK",
                    'L' => "KeyL",
                    'M' => "KeyM",
                    'N' => "KeyN",
                    'O' => "KeyO",
                    'P' => "KeyP",
                    'Q' => "KeyQ",
                    'R' => "KeyR",
                    'S' => "KeyS",
                    'T' => "KeyT",
                    'U' => "KeyU",
                    'V' => "KeyV",
                    'W' => "KeyW",
                    'X' => "KeyX",
                    'Y' => "KeyY",
                    _ => "KeyZ",
                },
                character as i64,
                true,
            ),
            '0'..='9' => (
                match character {
                    '0' => "Digit0",
                    '1' => "Digit1",
                    '2' => "Digit2",
                    '3' => "Digit3",
                    '4' => "Digit4",
                    '5' => "Digit5",
                    '6' => "Digit6",
                    '7' => "Digit7",
                    '8' => "Digit8",
                    _ => "Digit9",
                },
                character as i64,
                false,
            ),
            ' ' => ("Space", 32, false),
            '!' => ("Digit1", 49, true),
            '@' => ("Digit2", 50, true),
            '#' => ("Digit3", 51, true),
            '$' => ("Digit4", 52, true),
            '%' => ("Digit5", 53, true),
            '^' => ("Digit6", 54, true),
            '&' => ("Digit7", 55, true),
            '*' => ("Digit8", 56, true),
            '(' => ("Digit9", 57, true),
            ')' => ("Digit0", 48, true),
            '-' => ("Minus", 189, false),
            '_' => ("Minus", 189, true),
            '=' => ("Equal", 187, false),
            '+' => ("Equal", 187, true),
            '[' => ("BracketLeft", 219, false),
            '{' => ("BracketLeft", 219, true),
            ']' => ("BracketRight", 221, false),
            '}' => ("BracketRight", 221, true),
            '\\' => ("Backslash", 220, false),
            '|' => ("Backslash", 220, true),
            ';' => ("Semicolon", 186, false),
            ':' => ("Semicolon", 186, true),
            '\'' => ("Quote", 222, false),
            '"' => ("Quote", 222, true),
            ',' => ("Comma", 188, false),
            '<' => ("Comma", 188, true),
            '.' => ("Period", 190, false),
            '>' => ("Period", 190, true),
            '/' => ("Slash", 191, false),
            '?' => ("Slash", 191, true),
            '`' => ("Backquote", 192, false),
            '~' => ("Backquote", 192, true),
            _ => return None,
        };
        Some(definition)
    }

    /// `mode: "keystrokes"`：逐字符按键，而不是一次性插入。
    ///
    /// 每条 `keyDown` 都带着 `text`（CDP 收到文本会顺带产生 `input`），所以
    /// 页面既能看到 `keydown`/`keyup`，也能看到正常的一次字符输入 —— 两条通道
    /// 的事实一致，不会出现「值进去了但页面的按键状态没动」那种半真半假。
    ///
    /// 连续的非 ASCII 字符合并成一次 `Input.insertText`：它们没有物理按键，
    /// 逐字发一次 CDP 只是白等一圈超时窗口。
    ///
    /// 回两个数：`keys` 是真正走按键通道的字符数（每个都有 `keydown`/`keyup`
    /// 两条 CDP 调用），`inserted` 是被合并进 `insertText` 的字符数。分开报，
    /// 是因为两者对页面的可见度不同 —— 混成一个数，调用方就没法判断
    /// 「这段中文到底有没有触发页面的事件」。
    fn type_keystrokes(
        webview: &Webview,
        text: &str,
        delay_ms: u64,
    ) -> Result<(usize, usize), String> {
        let mut keys = 0;
        let mut inserted = 0;
        let mut pending = String::new();
        for character in text.chars() {
            let Some((code, virtual_key, shift)) = keystroke_key(character) else {
                pending.push(character);
                continue;
            };
            if !pending.is_empty() {
                insert_text(webview, &pending)?;
                inserted += pending.chars().count();
                pending.clear();
            }
            let modifiers = if shift { 8 } else { 0 };
            let text_value = character.to_string();
            let key_down = json!({
                "type": "keyDown",
                "key": text_value,
                "code": code,
                "windowsVirtualKeyCode": virtual_key,
                "nativeVirtualKeyCode": virtual_key,
                "text": text_value,
                "unmodifiedText": text_value,
                "modifiers": modifiers,
            });
            cdp_ok(webview, "Input.dispatchKeyEvent", &cdp_params(key_down))?;
            let key_up = json!({
                "type": "keyUp",
                "key": text_value,
                "code": code,
                "windowsVirtualKeyCode": virtual_key,
                "nativeVirtualKeyCode": virtual_key,
                "modifiers": modifiers,
            });
            cdp_ok(webview, "Input.dispatchKeyEvent", &cdp_params(key_up))?;
            keys += 1;
            if delay_ms > 0 {
                thread::sleep(Duration::from_millis(delay_ms));
            }
        }
        if !pending.is_empty() {
            insert_text(webview, &pending)?;
            inserted += pending.chars().count();
        }
        Ok((keys, inserted))
    }

    fn insert_text(webview: &Webview, text: &str) -> Result<(), String> {
        let params = json!({ "text": text });
        cdp_ok(webview, "Input.insertText", &cdp_params(params))
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

    /// 解掉一个被壳层接管的站点弹窗。
    ///
    /// 返回值里的 `bool` 是「这个标签名下有没有待决弹窗」：
    ///   * `Ok(true)`  —— 弹窗归壳层管，已经按 `accept` 处置并放行页面；
    ///   * `Ok(false)` —— 主线程上没有这个标签的手柄，调用方该走 CDP 那条路
    ///     （别的宿主弹的框，或者弹窗已经被超时兜底收掉了）；
    ///   * `Err(..)`   —— 手柄在，但放行失败；这个必须往上报，否则页面会
    ///     一直等下去。
    ///
    /// 手柄是按标签存在主线程 thread_local 里的 COM 接口，所以「取出 + 处置」
    /// 必须整段发生在 WebView2 自己的线程上 —— `with_webview` 正好提供这个时机。
    /// 提前在 worker 线程上探一下 map 是没用的：那是另一份 thread_local。
    fn complete_native_dialog(
        webview: &Webview,
        tab_id: &str,
        accept: bool,
        prompt_text: Option<String>,
    ) -> Result<bool, String> {
        let (sender, receiver) = mpsc::channel();
        let key = tab_id.to_string();
        webview
            .with_webview(move |_platform| {
                let _ = crate::crash_log::guard("browser.with-webview.dialog", move || {
                    let pending = PENDING_DIALOGS.with(|map| map.borrow_mut().remove(&key));
                    let Some(pending) = pending else {
                        let _ = sender.send(Ok(false));
                        return;
                    };
                    // 手柄已经取出来了，跨线程摘要必须同步作废：留着它，下一次
                    // 动作失败就会拿这条旧账解释新错。
                    clear_pending_dialog(&key);
                    let result = (|| -> Result<(), String> {
                        if accept {
                            if let Some(text) = prompt_text.as_deref() {
                                let value = HSTRING::from(text.to_string());
                                unsafe {
                                    pending
                                        .args
                                        .SetResultText(&value)
                                        .map_err(|error| format!("SetResultText failed: {error}"))?;
                                }
                            }
                            unsafe {
                                pending
                                    .args
                                    .Accept()
                                    .map_err(|error| format!("Accept failed: {error}"))?;
                            }
                        }
                        // `Complete()` 才是真正放行页面的那一下：没有它，
                        // 上面 Accept/Cancel 都只是写了个标记，渲染进程照样等。
                        unsafe {
                            pending
                                .deferral
                                .Complete()
                                .map_err(|error| format!("Deferral complete failed: {error}"))?;
                        }
                        Ok(())
                    })();
                    let _ = sender.send(result.map(|()| true));
                });
            })
            .map_err(|error| format!("Could not reach the native browser tab: {error}"))?;
        match receiver.recv_timeout(DIALOG_RESOLVE_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err("Timed out while closing the site dialog".to_string()),
        }
    }

    /// 面板上的一次驱动动作。
    ///
    /// `tab_id` 是给对话框用的：`ScriptDialogOpening` 攥下的 deferral 按标签存在
    /// 主线程上（`PENDING_DIALOGS`），收尾时必须知道自己在哪个标签里。
    fn perform_act(
        webview: &Webview,
        tab_id: &str,
        action: &str,
        message: &Value,
    ) -> Result<Value, String> {
        match action {
            "click" => {
                if input_route(message)? == "dom_event" {
                    let detail = dom_event_click(webview, message)?;
                    thread::sleep(Duration::from_millis(120));
                    return Ok(json!({ "inputRoute": "dom_event", "detail": detail }));
                }
                // 落点来自命中测试：中心被别的层挡住时，页面会把落点挪到
                // 「命中元素真的属于目标」的位置（见 `ELEMENT_POINT_SCRIPT`）。
                let target = act_point(webview, message)?;
                let (x, y) = (
                    target
                        .get("x")
                        .and_then(Value::as_f64)
                        .ok_or_else(|| "A valid elementRef or x/y point is required".to_string())?,
                    target
                        .get("y")
                        .and_then(Value::as_f64)
                        .ok_or_else(|| "A valid elementRef or x/y point is required".to_string())?,
                );
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
                // `effect:confirmed` 只说明事件发出去了；命中诊断交给上层判断有没有点歪。
                Ok(json!({
                    "inputRoute": "trusted",
                    "point": { "x": x, "y": y },
                    "hit": {
                        "onTarget": target.get("onTarget").cloned().unwrap_or(Value::Null),
                        "tag": target.get("hitTag").cloned().unwrap_or(Value::Null),
                        "href": target.get("hitHref").cloned().unwrap_or(Value::Null),
                    },
                    "probes": target.get("probes").cloned().unwrap_or(Value::Null),
                }))
            }
            "hover" | "move" => {
                let (x, y) = act_point_xy(webview, message)?;
                hover_at(webview, x, y)?;
                // 首帧兜底：页面刚重排过时，第一发 `mouseMoved` 偶尔没能把
                // `:hover` 落到目标上。这里问一句页面 —— 没落上就补发同样的一发，
                // 落上了就一分钱不花。幂等：补发不发第二遍，也不会改变动作语义。
                let mut rewarmed = false;
                if !hover_landed(webview, x, y) {
                    thread::sleep(Duration::from_millis(16));
                    hover_at(webview, x, y)?;
                    rewarmed = true;
                }
                Ok(json!({ "point": { "x": x, "y": y }, "rewarmed": rewarmed }))
            }
            "scroll" => {
                // `inputRoute` 在这里只做契约校验：滚动没有可信输入这条路
                // （见 `dom_event_scroll` 的说明），写法不认识照样要报出来。
                input_route(message)?;
                let detail = dom_event_scroll(webview, message)?;
                Ok(json!({ "inputRoute": "dom_event", "detail": detail }))
            }
            "select" => {
                // 和 `scroll` 同理：原生下拉展开后的列表不在页面的命中测试里，
                // 可信输入栈这条路走不通。`inputRoute` 只做契约校验，回执如实
                // 写 `dom_event`，不假装点的是真鼠标。
                input_route(message)?;
                let detail = select_option(webview, message)?;
                thread::sleep(Duration::from_millis(120));
                Ok(json!({ "inputRoute": "dom_event", "detail": detail }))
            }
            "type" => {
                let text = message
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "A text value is required".to_string())?;
                if text.len() > 20_000 {
                    return Err("Text value is too large".to_string());
                }
                // `mode` 对齐官方 `browser_type`：`insert_text` 是一次性插入，
                // `keystrokes` 是逐键。差别不在「文本进没进去」，而在页面能不能
                // 听到 `keydown` —— 下拉补全、搜索建议、按键判断只认后者。
                let mode = message
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("insert_text");
                if !matches!(mode, "insert_text" | "keystrokes") {
                    return Err(format!("Unsupported typing mode: {mode}"));
                }
                let replace = message
                    .get("replace")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if input_route(message)? == "dom_event" {
                    let detail = dom_event_type(webview, message)?;
                    thread::sleep(Duration::from_millis(120));
                    return Ok(json!({ "inputRoute": "dom_event", "detail": detail }));
                }
                let mut focused = false;
                if message.get("elementRef").and_then(Value::as_str).is_some()
                    || finite_point(message).is_some()
                {
                    let (x, y) = act_point_xy(webview, message)?;
                    perform_click(webview, x, y, "left", 1)?;
                    thread::sleep(Duration::from_millis(80));
                    focused = true;
                }
                // `replace` 只改选区、不换输入通道：文本仍然走可信输入栈，
                // 只是先让页面上已有的内容处于「被选中」状态，插入即替换。
                let replaced = if replace {
                    Some(select_all_text(webview)?)
                } else {
                    None
                };
                if mode == "keystrokes" {
                    let (keys, inserted) =
                        type_keystrokes(webview, text, keystroke_delay(message, text))?;
                    thread::sleep(Duration::from_millis(120));
                    return Ok(json!({
                        "mode": "keystrokes",
                        "keys": keys,
                        "inserted": inserted,
                        "length": text.chars().count(),
                        "replaced": replaced,
                        "focused": focused,
                    }));
                }
                insert_text(webview, text)?;
                thread::sleep(Duration::from_millis(120));
                Ok(json!({
                    "mode": "insert_text",
                    "length": text.chars().count(),
                    "replaced": replaced,
                    "focused": focused,
                }))
            }
            "key" | "press" => {
                let key = message
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "A key value is required".to_string())?;
                if input_route(message)? == "dom_event" {
                    let detail = dom_event_key(webview, message)?;
                    thread::sleep(Duration::from_millis(120));
                    return Ok(json!({ "inputRoute": "dom_event", "detail": detail }));
                }
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
                cdp_ok(webview, "Input.dispatchKeyEvent", &cdp_params(key_down))?;
                let key_up = json!({
                    "type": "keyUp",
                    "key": key,
                    "code": code,
                    "windowsVirtualKeyCode": virtual_key,
                    "nativeVirtualKeyCode": virtual_key,
                });
                cdp_ok(webview, "Input.dispatchKeyEvent", &cdp_params(key_up))?;
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
                let data = capture_png(webview)?;
                Ok(json!({ "dataUrl": format!("data:image/png;base64,{data}") }))
            }
            "snapshot" => snapshot(webview).map(|reply| json!({ "snapshot": reply })),
            "elements" => elements(webview, message).map(|reply| json!({ "elements": reply })),
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
                    // 「适配宽度」和「100%」都先把缩放摘回基准，区别在壳层：
                    // reset 之后由人继续掌控，fit 之后交还给自动适配。
                    Some("fit") => 1.0,
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
                find_in_page(webview, text, message)
            }
            "findStop" => {
                stop_find(webview)?;
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
                cdp_ok(webview, "Browser.setDownloadBehavior", &cdp_params(params))?;
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
            "drag" => {
                let (from_x, from_y) = drag_endpoint(webview, message, "from")?;
                let (to_x, to_y) = drag_endpoint(webview, message, "to")?;
                let steps = message
                    .get("steps")
                    .and_then(Value::as_u64)
                    .unwrap_or(12)
                    .clamp(1, 60);
                // `dataItems` 是 HTML5 拖放的货物清单。给了它，说明上层要的是
                // `dragstart`/`drop` 那一套 DOM 事件（看板排序、富文本拖拽），
                // 于是打开 CDP 的拖放拦截，由壳层替浏览器把这一趟走完；不给
                // 就是指针拖拽（滑块、画布、地图），真实鼠标事件序列就够了。
                let drag_data = message
                    .get("dataItems")
                    .and_then(Value::as_array)
                    .map(|entries| {
                        let items: Vec<Value> = entries
                            .iter()
                            .filter_map(|entry| {
                                let mime = entry.get("mimeType").and_then(Value::as_str)?;
                                if mime.len() > 120 {
                                    return None;
                                }
                                let data = entry.get("data").and_then(Value::as_str).unwrap_or("");
                                if data.len() > 100_000 {
                                    return None;
                                }
                                Some(json!({ "mimeType": mime, "data": data }))
                            })
                            .collect();
                        json!({ "items": items, "dragOperationsMask": 1 })
                    });
                let intercept = drag_data.is_some();
                if intercept {
                    let _ = cdp_ok(
                        webview,
                        "Input.setInterceptDrags",
                        &cdp_params(json!({ "enabled": true })),
                    );
                }
                let press = json!({
                    "type": "mousePressed",
                    "x": from_x,
                    "y": from_y,
                    "button": "left",
                    "buttons": 1,
                    "clickCount": 1,
                });
                cdp_ok(webview, "Input.dispatchMouseEvent", &cdp_params(press))?;
                for step in 1..=steps {
                    let progress = step as f64 / steps as f64;
                    let moved = json!({
                        "type": "mouseMoved",
                        "x": from_x + (to_x - from_x) * progress,
                        "y": from_y + (to_y - from_y) * progress,
                        "button": "left",
                        "buttons": 1,
                    });
                    cdp_ok(webview, "Input.dispatchMouseEvent", &cdp_params(moved))?;
                    thread::sleep(Duration::from_millis(12));
                }
                if let Some(data) = drag_data {
                    // 拦截模式下浏览器不再自己完成拖放：落点必须由壳层确认，
                    // 页面这才会收到 `dragover`/`drop`。
                    for (kind, pause) in [("dragEnter", 0_u64), ("dragOver", 60), ("drop", 0)] {
                        let event =
                            json!({ "type": kind, "x": to_x, "y": to_y, "data": data });
                        let _ = cdp_ok(webview, "Input.dispatchDragEvent", &cdp_params(event));
                        if pause > 0 {
                            thread::sleep(Duration::from_millis(pause));
                        }
                    }
                    let _ = cdp_ok(
                        webview,
                        "Input.setInterceptDrags",
                        &cdp_params(json!({ "enabled": false })),
                    );
                }
                let release = json!({
                    "type": "mouseReleased",
                    "x": to_x,
                    "y": to_y,
                    "button": "left",
                    "buttons": 0,
                    "clickCount": 1,
                });
                cdp_ok(webview, "Input.dispatchMouseEvent", &cdp_params(release))?;
                thread::sleep(Duration::from_millis(150));
                Ok(json!({
                    "from": { "x": from_x, "y": from_y },
                    "to": { "x": to_x, "y": to_y },
                    "steps": steps,
                    "dragData": intercept,
                }))
            }
            "upload" => {
                let files = upload_files(message)?;
                let object_id = upload_target_object(webview, message)?;
                let count = files.len();
                // 先 enable 一次是幂等的：`DOM.setFileInputFiles` 在部分运行时
                // 依赖 DOM 域已打开，而壳层不该去猜当前运行时的默认状态。
                let _ = call_cdp(webview, "DOM.enable", &cdp_params(json!({})));
                let params = json!({ "files": files, "objectId": object_id });
                cdp_ok(webview, "DOM.setFileInputFiles", &cdp_params(params))?;
                thread::sleep(Duration::from_millis(120));
                Ok(json!({ "files": count }))
            }
            "dialog" => {
                let mode = message.get("mode").and_then(Value::as_str).unwrap_or("accept");
                let accept = match mode {
                    "accept" | "ok" => true,
                    "dismiss" | "cancel" => false,
                    other => return Err(format!("Unsupported dialog mode: {other}")),
                };
                let prompt_text = message.get("promptText").and_then(Value::as_str);
                if let Some(text) = prompt_text {
                    if text.len() > 2000 {
                        return Err("Prompt text is too long".to_string());
                    }
                }
                // 壳层接管的弹窗：不走 CDP。
                //
                // 这条分支要排在前面不是性能优化，是正确性问题：页面被弹窗挡住
                // 的时候，`Runtime.*`/`DOM.*` 在这个标签上是叫不动的，只有
                // `Page.*` 还活着，而 `Page.handleJavaScriptDialog` 在
                // 「默认对话框已关 + 壳层攥着 deferral」这个状态下回的是
                // `No dialog is showing` —— 拿它当药方等于什么都没做。
                match complete_native_dialog(
                    webview,
                    tab_id,
                    accept,
                    prompt_text.map(str::to_string),
                ) {
                    Ok(true) => return Ok(json!({ "dialog": mode, "route": "native" })),
                    Ok(false) => {}
                    Err(error) => return Err(error),
                }
                // 没有壳层手柄：可能是别的宿主弹的框（官方截图路由），也可能
                // 这个弹窗已经被超时兜底收掉了。退回 CDP，并把「没有弹窗」
                // 明确报成错误 —— 上层据此重新观测，而不是重试同一下动作。
                let mut params = json!({ "accept": accept });
                if let Some(text) = prompt_text {
                    if let Some(object) = params.as_object_mut() {
                        object.insert("promptText".to_string(), json!(text));
                    }
                }
                let raw = call_cdp(webview, "Page.handleJavaScriptDialog", &cdp_params(params))
                    .map_err(|_| "No JavaScript dialog is waiting".to_string())?;
                // CDP 把「根本没有弹窗」也当成一次成功的调用，只在返回体里带
                // `error`。只看有没有回包会把这种情况误判成处理成功。
                if let Ok(reply) = serde_json::from_str::<Value>(&raw) {
                    if reply.get("error").is_some() {
                        return Err("No JavaScript dialog is waiting".to_string());
                    }
                }
                Ok(json!({ "dialog": mode, "route": "cdp" }))
            }
            _ => Err(format!("Unsupported action: {action}")),
        }
    }

    /// 动作可视化：把「这一下落在哪」投给页面里的可视化层（见 `TAB_INIT_SCRIPT`）。
    ///
    /// 画在页面里而不是面板里，是因为原生子 WebView2 永远画在 dashboard 的 HTML
    /// 之上 —— 画在面板上的光标会被网页整块盖住。这一层是装饰：投递失败不记成
    /// 动作失败，页面读不到就下次再说。
    fn visualize_act(webview: &Webview, action: &str, message: &Value, detail: &Value) {
        // `scroll` / `select` 走的是 `dom_event` 通道，回执里还套着一层 `detail`：
        // 落点与位移在那层里，先摊平再读。
        let inner = detail.get("detail").unwrap_or(detail);
        let mut spec = serde_json::Map::new();
        spec.insert("action".to_string(), json!(action));
        if let Some(reference) = message.get("elementRef").and_then(Value::as_str) {
            if valid_element_ref(reference) {
                spec.insert("ref".to_string(), json!(reference));
            }
        }
        if let Some((x, y)) = finite_point(message) {
            spec.insert("x".to_string(), json!(x));
            spec.insert("y".to_string(), json!(y));
        }
        // 可信输入那条路把落点写在回执里（`point` / `from` / `to`）：参数里可能
        // 只有 elementRef，落点得从回执里取。回执优先，参数兜底。
        for key in ["point", "from", "to"] {
            let value = detail
                .get(key)
                .or_else(|| inner.get(key))
                .or_else(|| message.get(key));
            if let Some(value) = value {
                if value.is_object() {
                    spec.insert(key.to_string(), value.clone());
                }
            }
        }
        // 滚动的位移三种写法都收，键名统一成 `deltaX` / `deltaY` 再交给页面。
        for (source, canonical) in [
            ("deltaX", "deltaX"),
            ("deltaY", "deltaY"),
            ("dx", "deltaX"),
            ("dy", "deltaY"),
        ] {
            if spec.contains_key(canonical) {
                continue;
            }
            let value = inner.get(source).or_else(|| message.get(source));
            if let Some(value) = value {
                if value.as_f64().is_some() {
                    spec.insert(canonical.to_string(), value.clone());
                }
            }
        }
        let script = format!(
            "(window.__starshipVisual && window.__starshipVisual.act({}), true)",
            js_literal(&Value::Object(spec))
        );
        let _ = execute_script(webview, script);
    }

    /// 给页面盖一段「这段时间里的输入算智能体自己的」窗口。
    ///
    /// 页面的「人手优先」探针（`TAB_INIT_SCRIPT` 结尾）只认 `event.isTrusted`，
    /// 而 CDP 的 `Input.*` 派出来的事件**也是**可信事件 —— 不给它盖个章，智能体
    /// 会把自己认成用户，然后被自己锁在门外。窗口长度必须比动作本身活得久：CDP
    /// 的输入事件落到渲染进程是异步的，回执回来时事件可能还在路上。
    ///
    /// **不等回执**。这条写入只是给页面里的探针看的，而它和后面紧跟着的
    /// `CallDevToolsProtocolMethod` 走的是同一个 WebView2 线程队列，顺序天然靠
    /// 得住；反过来，页面被站点弹窗冻住时 `ExecuteScript` 要等满十秒超时，让
    /// 一次动作白搭十秒是划不来的。
    fn arm_agent_input(webview: &Webview, until_ms: i64) {
        let javascript = HSTRING::from(format!("window.__starshipAgentInputUntil = {until_ms};"));
        let _ = webview.with_webview(move |platform| {
            let _ = crate::crash_log::guard("browser.arm-agent-input", move || {
                let Ok(core) = (unsafe { platform.controller().CoreWebView2() }) else {
                    return;
                };
                let handler = ExecuteScriptCompletedHandler::create(guarded_completed(
                    "browser.arm-agent-input-completed",
                    move |_error: windows::core::Result<()>, _result: String| Ok(()),
                ));
                let _ = unsafe { core.ExecuteScript(&javascript, &handler) };
            });
        });
    }

    /// 这条动作会不会往页面里灌真实输入事件。
    ///
    /// 只有这些动作需要在前后盖章：`snapshot` / `screenshot` / `wait` 之类不碰
    /// 输入，盖了只会平白吃掉用户一段操作。
    fn act_injects_input(action: &str) -> bool {
        matches!(
            action,
            "click" | "hover" | "move" | "drag" | "scroll" | "type" | "select" | "key" | "press"
        )
    }

    /// `dispatch` 这条通道上会往页面灌输入的那几条 CDP 命令。
    ///
    /// 观察类命令（`Runtime.*` / `DOM.*` / `Page.*`）从来不碰输入，让它们照常走 ——
    /// 用户正在页面上打字的时候，智能体本该还能看一眼页面发生了什么。
    fn dispatch_injects_input(method: &str) -> bool {
        const INJECTORS: [&str; 7] = [
            "Input.dispatchMouseEvent",
            "Input.dispatchKeyEvent",
            "Input.dispatchTouchEvent",
            "Input.dispatchDragEvent",
            "Input.insertText",
            "Input.synthesizeScrollGesture",
            "Input.synthesizePinchGesture",
        ];
        INJECTORS.contains(&method)
    }

    /// 这一下是往「当前有焦点的元素」里送按键吗。
    ///
    /// 键盘类动作不认坐标：按键只会落到有焦点的那个元素上，所以判据只能是
    /// 「用户是不是正在写」，不是「他点在哪儿」。
    fn act_reads_keyboard(action: &str) -> bool {
        matches!(action, "type" | "key" | "press")
    }

    /// `dispatch` 通道上的键盘类命令（同上）。
    fn dispatch_reads_keyboard(method: &str) -> bool {
        matches!(method, "Input.dispatchKeyEvent" | "Input.insertText")
    }

    /// 这一下会动视口吗。滚动和滚动撞在一起一定互相顶掉，所以不走坐标判定。
    fn act_moves_viewport(action: &str) -> bool {
        action == "scroll"
    }

    /// `dispatch` 通道上的视口类命令（同上）。
    fn dispatch_moves_viewport(method: &str) -> bool {
        matches!(
            method,
            "Input.synthesizeScrollGesture" | "Input.synthesizePinchGesture"
        )
    }

    /// 用户那一笔和智能体这一下是不是同一块地方。
    ///
    /// 满足其一就算：命中同一个元素（`data-starship-ref` 相同），或者落点相距
    /// 在 `HUMAN_TOUCH_RADIUS_PX` 以内。两边都缺现场信息时返回 `false` ——
    /// 认不出来就别挡，挡错了（智能体明明没碰用户的地方却停住）比放过去更伤。
    fn same_target(
        human: &HumanInput,
        point: Option<(f64, f64)>,
        reference: Option<&str>,
    ) -> bool {
        if let (Some(agent), Some(human_target)) = (reference, human.reference.as_deref()) {
            if agent == human_target {
                return true;
            }
        }
        match (point, human.point) {
            (Some((ax, ay)), Some((hx, hy))) => {
                (ax - hx).hypot(ay - hy) <= HUMAN_TOUCH_RADIUS_PX
            }
            _ => false,
        }
    }

    /// 从动作参数里读智能体的落点。
    ///
    /// 认三种写法：顶层 `x`/`y`，`drag` 的 `from`/`to`，以及 `point` 子对象。
    /// 读不到就返回 `None`（`drag` 这种只有 `elementRef` 的写法就是），判定
    /// 回落到「同一个元素」那一条。
    ///
    /// 和 `perform_act` 的 `act_point` 是两码事：那个要拿 WebView 去页面上量
    /// 元素中心的真实坐标，会等一次脚本往返；这个只读报文里已经写着的数，
    /// 一个字段都不能等 —— 判定在动作之前，页面卡住的时候等不起。
    fn human_target_point(message: &Value) -> Option<(f64, f64)> {
        if let Some(point) = finite_point(message) {
            return Some(point);
        }
        for key in ["point", "from", "to"] {
            if let Some(point) = message.get(key).and_then(finite_point) {
                return Some(point);
            }
        }
        None
    }

    /// 从动作参数里读智能体瞄的元素（`elementRef`，含 `drag` 的成对写法）。
    fn act_reference(message: &Value) -> Option<&str> {
        if let Some(reference) = message.get("elementRef").and_then(Value::as_str) {
            return Some(reference);
        }
        for key in ["point", "from", "to"] {
            if let Some(reference) = message
                .get(key)
                .and_then(|scope| scope.get("elementRef"))
                .and_then(Value::as_str)
            {
                return Some(reference);
            }
        }
        None
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

    fn capture_png(webview: &Webview) -> Result<String, String> {
        capture_frame(webview, r#"{"format":"png"}"#)
    }

    /// 站点图标，取回来就是 data URL（拿不到给空串）。
    ///
    /// 为什么要在标签页里取、而不是在 dashboard 里贴一个 `<img>`：dashboard 的
    /// CSP 会把外链图片拦掉，只有内联的 data URL 画得出来。而在页面上下文里用
    /// `fetch` 读图标同样危险 —— 站点的 `connect-src` 会拦跨源请求。所以候选
    /// 顺序是「同源优先」，同源读不到才试跨源（对方开了 CORS 才能成）。
    ///
    /// 用 `Runtime.evaluate` 而不是 `execute_script`：后者不等 Promise，返回的
    /// 是 `{}`。`Runtime.` 在 `allowed_cdp_method` 的白名单里。
    fn capture_favicon_data_url(webview: &Webview) -> Option<String> {
        const SCRIPT: &str = r#"(function () {
  var origin = location.origin;
  var candidates = [];
  var links = document.querySelectorAll('link[rel]');
  for (var i = 0; i < links.length; i += 1) {
    var rel = (links[i].getAttribute('rel') || '').toLowerCase();
    if (rel.indexOf('icon') === -1) { continue; }
    var href = links[i].getAttribute('href');
    if (!href) { continue; }
    try { candidates.push(new URL(href, location.href).href); } catch (error) { }
  }
  if (origin && origin !== 'null') { candidates.push(origin + '/favicon.ico'); }
  candidates.sort(function (a, b) {
    return (a.indexOf(origin) === 0 ? 0 : 1) - (b.indexOf(origin) === 0 ? 0 : 1);
  });
  var unique = [];
  for (var u = 0; u < candidates.length && unique.length < 6; u += 1) {
    if (unique.indexOf(candidates[u]) === -1) { unique.push(candidates[u]); }
  }
  var read = function (url) {
    return fetch(url, { credentials: 'omit', mode: 'cors' }).then(function (response) {
      if (!response.ok) { return ''; }
      return response.blob().then(function (blob) {
        if (!blob || blob.size === 0 || blob.size > 200000) { return ''; }
        return new Promise(function (resolve) {
          var reader = new FileReader();
          reader.onload = function () { resolve(String(reader.result || '')); };
          reader.onerror = function () { resolve(''); };
          reader.readAsDataURL(blob);
        });
      });
    }).catch(function () { return ''; });
  };
  var step = function (index) {
    if (index >= unique.length) { return Promise.resolve(''); }
    return read(unique[index]).then(function (data) {
      return data ? data : step(index + 1);
    });
  };
  return step(0);
})()"#;
        let params = json!({
            "expression": SCRIPT,
            "awaitPromise": true,
            "returnByValue": true,
        });
        let raw = call_cdp(webview, "Runtime.evaluate", &cdp_params(params)).ok()?;
        let parsed: Value = serde_json::from_str(&raw).ok()?;
        let data = parsed.get("result")?.get("value")?.as_str()?;
        // 只认内联图片，且设一个上限：图标本来就是几 KB，超大的一律不要。
        if !data.starts_with("data:image/") || data.len() > 400_000 {
            return None;
        }
        Some(data.to_string())
    }

    /// 遮挡替身用的那一帧：原生子视图让位（hide）之前先把它现在的画面截下来，
    /// 贴回面板原位，菜单就不会把网页切成一片空白。JPEG 比 PNG 小一个数量级，
    /// 而这张图只在菜单开着的那一瞬间当背景板用，看得清是刚才那一页就够了。
    fn capture_preview(webview: &Webview) -> Option<String> {
        let data = capture_frame(webview, r#"{"format":"jpeg","quality":62}"#).ok()?;
        Some(format!("data:image/jpeg;base64,{data}"))
    }

    /// 截一帧。**不再吞错**：老的完成回调把 `HRESULT` 丢掉，于是「超时 / 没帧 / 被拒」
    /// 三种完全不同的原因在调用方看来都是同一句「截图失败」，日志里也一个字没有。
    fn capture_frame(webview: &Webview, parameters: &'static str) -> Result<String, String> {
        let (sender, receiver) = mpsc::channel();
        webview
            .with_webview(move |platform| {
                let _ = crate::crash_log::guard("browser.with-webview.capture-png", move || {
                let core = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => core,
                    Err(error) => {
                        let _ = sender.send(Err(format!("CoreWebView2 unavailable: {error}")));
                        return;
                    }
                };
                let handler_sender = sender.clone();
                let handler = CallDevToolsProtocolMethodCompletedHandler::create(
                    guarded_completed(
                        "browser.capture-screenshot-completed",
                        move |error: windows::core::Result<()>, result: String| {
                        // 结果体里是整张 base64 PNG，出错时才截前 200 字符进日志。
                        let outcome = if error.is_ok() {
                            serde_json::from_str::<Value>(&result)
                                .ok()
                                .and_then(|value| {
                                    value.get("data").and_then(Value::as_str).map(str::to_string)
                                })
                                .ok_or_else(|| {
                                    format!(
                                        "Page.captureScreenshot returned no data: {}",
                                        result.chars().take(200).collect::<String>()
                                    )
                                })
                        } else {
                            Err(format!(
                                "Page.captureScreenshot: {error:?}; body={}",
                                result.chars().take(200).collect::<String>()
                            ))
                        };
                        let _ = handler_sender.send(outcome);
                        Ok(())
                        },
                    ),
                );
                let method = HSTRING::from("Page.captureScreenshot");
                let parameters = HSTRING::from(parameters);
                if unsafe { core.CallDevToolsProtocolMethod(&method, &parameters, &handler) }
                    .is_err()
                {
                    let _ = sender.send(Err("Page.captureScreenshot rejected".to_string()));
                }
                });
            })
            .map_err(|error| format!("WebView2 unavailable: {error}"))?;
        let outcome = receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "Page.captureScreenshot timed out after 10s".to_string())?;
        if let Err(error) = &outcome {
            bridge_log(&format!("capture failed: {error}"));
        }
        outcome
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

    /// 打开一条已经落盘的下载：`reveal` 是「在文件夹里选中」，否则交给系统默认
    /// 程序打开。
    ///
    /// 只认下载目录里**确实存在**的文件。这条命令的手柄来自面板（也就是网页
    /// 侧），拿到任意路径就去 ShellExecute，等于把本机文件系统交出去。
    fn open_download_path(path: &str, reveal: bool) -> Result<(), String> {
        let target = std::path::PathBuf::from(path);
        if !target.is_absolute() {
            return Err("Download path must be absolute".to_string());
        }
        let metadata = std::fs::metadata(&target)
            .map_err(|error| format!("Download is gone: {error}"))?;
        if !metadata.is_file() {
            return Err("Download path is not a file".to_string());
        }
        let directory = download_directory(None)?;
        let root = std::fs::canonicalize(&directory).unwrap_or(directory);
        let resolved = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
        if !resolved.starts_with(&root) {
            return Err("Download lives outside the download directory".to_string());
        }
        // explorer `/select,` 能在文件夹里把文件选中；`cmd /C start` 后面的空串
        // 是 start 自己的「窗口标题」占位，少了它，带空格或引号的路径会被当标题。
        let spawned = if reveal {
            std::process::Command::new("explorer")
                .arg(format!("/select,{}", resolved.to_string_lossy()))
                .spawn()
        } else {
            std::process::Command::new("cmd")
                .args(["/C", "start", ""])
                .arg(&resolved)
                .spawn()
        };
        spawned.map(|_| ()).map_err(|error| error.to_string())
    }

    /// `FOLDERID_Downloads`：Windows 的「下载」是一个已知文件夹，用户可以在资源
    /// 管理器里把它挪到别的盘（本机就是 `D:\星舰・起源\Downloads`）。
    const DOWNLOADS_FOLDER_ID: &str = "{374DE290-123F-4565-9164-39C4925E467B}";
    const USER_SHELL_FOLDERS: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders";
    const HKEY_CURRENT_USER: isize = -2_147_483_647;
    /// `RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ`：不带 `RRF_NOEXPAND`，所以
    /// `RegGetValueW` 会把 `%USERPROFILE%\Downloads` 这类可展开字符串换成绝对
    /// 路径再交回来。
    const RRF_RT_STRING: u32 = 0x0000_0002 | 0x0000_0004;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_MORE_DATA: i32 = 234;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            hkey: isize,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut std::ffi::c_void,
            size: *mut u32,
        ) -> i32;
    }

    /// 读注册表里 Downloads 那条已知文件夹；键或值不在就回 `None`，调用方退到
    /// `%USERPROFILE%\Downloads`。
    fn read_downloads_registry_value() -> Option<String> {
        fn wide(value: &str) -> Vec<u16> {
            value.encode_utf16().chain(std::iter::once(0)).collect()
        }

        let subkey = wide(USER_SHELL_FOLDERS);
        let name = wide(DOWNLOADS_FOLDER_ID);
        let mut units = 512usize;
        loop {
            let mut buffer = vec![0u16; units];
            let mut size = (buffer.len() * 2) as u32;
            // SAFETY: 两个字符串都是 NUL 结尾的 UTF-16 缓冲，数据缓冲区按 `size`
            // 报出的字节数分配，同一个 `size` 又是它的容量，所以内核写不满。
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    name.as_ptr(),
                    RRF_RT_STRING,
                    std::ptr::null_mut(),
                    buffer.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            if status == ERROR_SUCCESS {
                let taken = ((size as usize) / 2).min(buffer.len());
                let text = String::from_utf16_lossy(&buffer[..taken]);
                return Some(text.trim_end_matches('\0').to_string());
            }
            // 值比缓冲区长：内核把需要的字节数写回 `size`，按它再要一次。上限
            // 只是防呆，正常的下载路径到不了 16 K 个 UTF-16 单元。
            if status == ERROR_MORE_DATA && units < 16 * 1024 {
                units = ((size as usize) / 2 + 1).max(units * 2);
                continue;
            }
            return None;
        }
    }

    /// Windows 上「下载」到底在哪，只有注册表知道：用户把文件夹重定向到别的盘
    /// 以后（本机就在 `D:\星舰・起源\Downloads`），`%USERPROFILE%\Downloads`
    /// **通常仍然存在**，所以「目录在不在」这种检查永远抓不到这个错，落盘的下载
    /// 会出现在别处，而 `open_download_path` 的包含性校验会把面板发来的真实路径
    /// 当成越权路径拒掉——表现就是下载列表里的文件点不开、「打开文件夹」开到
    /// 一个空目录。
    ///
    /// 也别去读隔壁的 `Shell Folders`：那一份在本机是过期的
    /// `C:\Users\36042\Downloads`，`User Shell Folders` 才是权威值。
    fn known_downloads_dir() -> Option<std::path::PathBuf> {
        downloads_dir_from_registry_value(read_downloads_registry_value())
    }

    /// 注册表里的值可能是绝对路径，也可能是 `%USERPROFILE%\Downloads` 这类可展开
    /// 字符串（手改过的机器上仍然常见）。空的和相对路径都当成不可用。
    fn downloads_dir_from_registry_value(value: Option<String>) -> Option<std::path::PathBuf> {
        let expanded = expand_windows_env(value?.trim());
        let path = std::path::PathBuf::from(expanded.trim());
        if path.as_os_str().is_empty() || !path.is_absolute() {
            return None;
        }
        Some(path)
    }

    /// `%NAME%` 逐个换成进程环境里的值；查不到的变量原样留着，落单的 `%` 也不
    /// 吞字符——`ExpandEnvironmentStrings` 就是这个口径。
    fn expand_windows_env(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        let mut rest = value;
        while let Some(start) = rest.find('%') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            match after.find('%') {
                Some(0) => {
                    // `%%` 里没有变量名，两个字符都留着。
                    out.push_str("%%");
                    rest = &after[1..];
                }
                Some(end) => {
                    let name = &after[..end];
                    match std::env::var(name) {
                        Ok(replacement) => out.push_str(&replacement),
                        Err(_) => {
                            out.push('%');
                            out.push_str(name);
                            out.push('%');
                        }
                    }
                    rest = &after[end + 1..];
                }
                None => {
                    out.push('%');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Downloads land in the user's Downloads folder unless the caller names a
    /// directory, so the shell never has to guess where a file went. 兜底顺序是
    /// 「系统记录的已知文件夹 → `%USERPROFILE%\Downloads`」：注册表那条读不到，
    /// 或者指向一个建不出来的目录（拔掉的移动盘），才退到后者。
    fn download_directory(requested: Option<&str>) -> Result<std::path::PathBuf, String> {
        let candidates = match requested {
            Some(value) if !value.trim().is_empty() => vec![std::path::PathBuf::from(value)],
            _ => download_directory_candidates(known_downloads_dir(), std::env::var("USERPROFILE").ok()),
        };
        if candidates.is_empty() {
            return Err("USERPROFILE is not set".to_string());
        }
        let mut failure = String::new();
        for path in candidates {
            if path.as_os_str().len() > 260 {
                failure = "Download path is too long".to_string();
                continue;
            }
            match std::fs::create_dir_all(&path) {
                Ok(()) => return Ok(path),
                Err(error) => {
                    failure = format!("Could not create the download folder: {error}");
                }
            }
        }
        Err(if failure.is_empty() {
            "No download folder is available".to_string()
        } else {
            failure
        })
    }

    /// 已知文件夹优先，`%USERPROFILE%\Downloads` 只是兜底。抽出来是为了让单测
    /// 钉住这个顺序，不用真去改这台机器的注册表。
    fn download_directory_candidates(
        known: Option<std::path::PathBuf>,
        profile: Option<String>,
    ) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();
        if let Some(path) = known {
            paths.push(path);
        }
        if let Some(profile) = profile.filter(|value| !value.trim().is_empty()) {
            let fallback = std::path::PathBuf::from(profile).join("Downloads");
            if !paths.contains(&fallback) {
                paths.push(fallback);
            }
        }
        paths
    }

    /// Serializes endpoint startup: two rapid dashboard requests must never
    /// launch two browsers against the same profile directory.
    static ENSURE_TASK_BROWSER: Mutex<()> = Mutex::new(());

    /// How long the boot may take before the shell gives up on a freshly
    /// spawned browser and answers the dashboard with a failure.
    const TASK_BROWSER_BOOT_TIMEOUT: Duration = Duration::from_millis(12_000);
    const TASK_BROWSER_BOOT_POLL: Duration = Duration::from_millis(100);
    const TASK_BROWSER_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

    /// Local attach-only profile the official browser panel is wired to.
    struct AttachProfile {
        name: String,
        port: u16,
        user_data_dir: std::path::PathBuf,
        executable: Option<String>,
    }

    /// `~/.openclaw` (or the state/config overrides), mirroring the Gateway's
    /// own `resolveConfigDir` so the shell reads the same file the panel does.
    fn openclaw_config_dir() -> Option<std::path::PathBuf> {
        if let Ok(state) = std::env::var("OPENCLAW_STATE_DIR") {
            let state = state.trim();
            if !state.is_empty() {
                return Some(std::path::PathBuf::from(state));
            }
        }
        if let Ok(config_path) = std::env::var("OPENCLAW_CONFIG_PATH") {
            let config_path = config_path.trim();
            if !config_path.is_empty() {
                return std::path::PathBuf::from(config_path)
                    .parent()
                    .map(|parent| parent.to_path_buf());
            }
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            if !profile.trim().is_empty() {
                return Some(std::path::PathBuf::from(profile).join(".openclaw"));
            }
        }
        let drive = std::env::var("HOMEDRIVE").ok()?;
        let path = std::env::var("HOMEPATH").ok()?;
        let combined = format!("{drive}{path}");
        if combined.trim().is_empty() {
            return None;
        }
        Some(std::path::PathBuf::from(combined).join(".openclaw"))
    }

    /// The profile the panel talks to, but only when it is a loopback
    /// `attachOnly` profile. Everything else (a Gateway-managed browser, the
    /// Chrome extension, an existing user session, a remote CDP host) is owned
    /// by the Gateway or by the user, and the shell must keep its hands off.
    fn attach_only_profile() -> Option<AttachProfile> {
        let directory = openclaw_config_dir()?;
        let text = std::fs::read_to_string(directory.join("openclaw.json")).ok()?;
        let config: Value = serde_json::from_str(&text).ok()?;
        let browser = config.get("browser")?;
        let name = browser
            .get("defaultProfile")
            .and_then(Value::as_str)
            .unwrap_or("openclaw")
            .to_string();
        let profile = browser.get("profiles")?.get(&name)?;
        if profile.get("driver").and_then(Value::as_str).unwrap_or("openclaw") != "openclaw" {
            return None;
        }
        if !profile
            .get("attachOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return None;
        }
        let port = match profile.get("cdpUrl").and_then(Value::as_str) {
            Some(cdp_url) => {
                let parsed = Url::parse(cdp_url).ok()?;
                if !matches!(
                    parsed.host_str().unwrap_or_default(),
                    "127.0.0.1" | "localhost" | "::1" | "[::1]"
                ) {
                    return None;
                }
                parsed.port()?
            }
            None => match profile.get("cdpPort").and_then(Value::as_u64) {
                Some(port) if (1..=65535).contains(&port) => port as u16,
                _ => return None,
            },
        };
        let user_data_dir = match profile.get("userDataDir").and_then(Value::as_str) {
            Some(value) if !value.trim().is_empty() => std::path::PathBuf::from(value),
            // Official layout for a managed OpenClaw profile
            // (`resolveOpenClawUserDataDir` in chrome.ts).
            _ => directory.join("browser").join(&name).join("user-data"),
        };
        Some(AttachProfile {
            name,
            port,
            user_data_dir,
            executable: profile
                .get("executablePath")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    browser
                        .get("executablePath")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }),
        })
    }

    fn join_root(root: &str, segments: &[&str]) -> std::path::PathBuf {
        let mut path = std::path::PathBuf::from(root);
        for segment in segments {
            path.push(segment);
        }
        path
    }

    /// Chromium-family candidates in the same order the official resolver
    /// walks on Windows (`chrome.executables.ts`): per-user installs first,
    /// then Program Files, Edge preferred on this shell because the panel was
    /// validated against it and it ships with Windows.
    fn browser_executable_candidates(configured: Option<&str>) -> Vec<std::path::PathBuf> {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Some(path) = configured {
            if !path.trim().is_empty() {
                candidates.push(std::path::PathBuf::from(path));
            }
        }
        let installs: [&[&str]; 3] = [
            &["Microsoft", "Edge", "Application", "msedge.exe"],
            &["Google", "Chrome", "Application", "chrome.exe"],
            &["BraveSoftware", "Brave-Browser", "Application", "brave.exe"],
        ];
        let mut roots: Vec<String> = Vec::new();
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            roots.push(local);
        }
        for name in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(root) = std::env::var(name) {
                roots.push(root);
            }
        }
        for root in &roots {
            for segments in installs {
                candidates.push(join_root(root, segments));
            }
        }
        candidates
    }

    fn cdp_port_open(port: u16) -> bool {
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        std::net::TcpStream::connect_timeout(&address, TASK_BROWSER_PROBE_TIMEOUT).is_ok()
    }

    /// Launch the endpoint the panel attaches to. The window is headless on
    /// purpose: the page the user sees is the shell's own WebView2 child view,
    /// and this process only has to provide the CDP target the Gateway and the
    /// `browser` tool drive. A visible Edge window would be a second, unowned
    /// browser window on the desktop.
    fn spawn_attach_browser(
        profile: &AttachProfile,
        executable: &std::path::Path,
    ) -> Result<(), String> {
        let mut command = std::process::Command::new(executable);
        command
            .arg(format!("--remote-debugging-port={}", profile.port))
            .arg(format!("--user-data-dir={}", profile.user_data_dir.display()))
            .arg("--headless=new")
            .arg("--disable-gpu")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-sync")
            .arg("--disable-background-networking")
            .arg("--disable-component-update")
            .arg("--disable-features=Translate,MediaRouter")
            .arg("--disable-session-crashed-bubble")
            .arg("--hide-crash-restore-bubble")
            .arg("--password-store=basic")
            .arg("--no-proxy-server")
            .arg("about:blank")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // A console window for a helper process is never acceptable here.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not start the browser: {error}"))
    }

    /// Make sure the attach-only CDP endpoint is listening, launching the local
    /// Chromium-family browser when it is not.
    ///
    /// Why the shell has to do this: the official panel's "Start browser"
    /// button asks the Gateway to start the profile, and the Gateway refuses
    /// every attach-only profile with `Browser attachOnly is enabled and
    /// profile "task-browser" is not running.` The Gateway never launches such
    /// a profile by design, so unless something else owns the port the panel
    /// stays on its empty state forever.
    fn ensure_task_browser() -> Result<u16, String> {
        let _guard = match ENSURE_TASK_BROWSER.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(profile) = attach_only_profile() else {
            return Err("The configured browser profile is not an attach-only local profile.".to_string());
        };
        if cdp_port_open(profile.port) {
            return Ok(profile.port);
        }
        let executable = browser_executable_candidates(profile.executable.as_deref())
            .into_iter()
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| "No Chromium-family browser was found to attach to.".to_string())?;
        bridge_log(&format!(
            "attach browser: starting {} on port {} (profile={})",
            executable.display(),
            profile.port,
            profile.name
        ));
        spawn_attach_browser(&profile, &executable)?;
        let started = Instant::now();
        while started.elapsed() < TASK_BROWSER_BOOT_TIMEOUT {
            if cdp_port_open(profile.port) {
                bridge_log(&format!(
                    "attach browser: port {} ready in {}ms",
                    profile.port,
                    started.elapsed().as_millis()
                ));
                return Ok(profile.port);
            }
            thread::sleep(TASK_BROWSER_BOOT_POLL);
        }
        Err(format!(
            "The browser did not open its debug port ({}) in time.",
            profile.port
        ))
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

    #[cfg(test)]
    mod download_directory_tests {
        use super::{
            download_directory_candidates, downloads_dir_from_registry_value, expand_windows_env,
        };
        use std::path::PathBuf;

        #[test]
        fn downloads_prefers_the_known_folder_over_the_profile_fallback() {
            let known = PathBuf::from(r"D:\星舰・起源\Downloads");
            let paths = download_directory_candidates(
                Some(known.clone()),
                Some(r"C:\Users\fixture".to_string()),
            );
            assert_eq!(
                paths,
                vec![known, PathBuf::from(r"C:\Users\fixture\Downloads")]
            );
        }

        #[test]
        fn downloads_falls_back_to_the_profile_only_without_a_known_folder() {
            let paths = download_directory_candidates(None, Some(r"C:\Users\fixture".to_string()));
            assert_eq!(paths, vec![PathBuf::from(r"C:\Users\fixture\Downloads")]);
            assert!(download_directory_candidates(None, None).is_empty());
            assert!(download_directory_candidates(None, Some("  ".to_string())).is_empty());
            // 两条路径重合时不该问同一个目录两遍。
            let same = download_directory_candidates(
                Some(PathBuf::from(r"C:\Users\fixture\Downloads")),
                Some(r"C:\Users\fixture".to_string()),
            );
            assert_eq!(same, vec![PathBuf::from(r"C:\Users\fixture\Downloads")]);
        }

        #[test]
        fn a_redirected_known_folder_survives_the_registry_round_trip() {
            // 本机就是这样：`User Shell Folders` 里写着别的盘上的中文路径。
            let redirected = r"D:\星舰・起源\Downloads".to_string();
            assert_eq!(
                downloads_dir_from_registry_value(Some(redirected.clone())),
                Some(PathBuf::from(redirected))
            );
        }

        #[test]
        fn an_expandable_registry_value_is_expanded() {
            // `RegGetValueW` 已经展开过一次，手改过的机器上仍可能留下变量。
            std::env::set_var("STARSHIP_DOWNLOAD_TEST_ROOT", r"D:\fixture");
            assert_eq!(
                downloads_dir_from_registry_value(Some(
                    r"%STARSHIP_DOWNLOAD_TEST_ROOT%\Downloads".to_string()
                )),
                Some(PathBuf::from(r"D:\fixture\Downloads"))
            );
        }

        #[test]
        fn blank_or_relative_registry_values_are_rejected() {
            assert_eq!(downloads_dir_from_registry_value(None), None);
            assert_eq!(
                downloads_dir_from_registry_value(Some("   ".to_string())),
                None
            );
            assert_eq!(
                downloads_dir_from_registry_value(Some(r"Downloads".to_string())),
                None
            );
        }

        #[test]
        fn an_unknown_variable_stays_literal() {
            assert_eq!(
                expand_windows_env(r"%STARSHIP_NO_SUCH_VAR_9X%\Downloads"),
                r"%STARSHIP_NO_SUCH_VAR_9X%\Downloads"
            );
            assert_eq!(expand_windows_env(r"C:\plain"), r"C:\plain");
            assert_eq!(expand_windows_env(r"50%"), r"50%");
            assert_eq!(expand_windows_env(r"100%%done"), r"100%%done");
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::{install, NativeBrowserState, INIT_SCRIPT, TAB_INIT_SCRIPT};

#[cfg(not(target_os = "windows"))]
pub const INIT_SCRIPT: &str = "";

#[cfg(not(target_os = "windows"))]
pub const TAB_INIT_SCRIPT: &str = "";

#[cfg(not(target_os = "windows"))]
#[derive(Default)]
pub struct NativeBrowserState;

#[cfg(not(target_os = "windows"))]
pub fn install(_app: tauri::AppHandle) {}
